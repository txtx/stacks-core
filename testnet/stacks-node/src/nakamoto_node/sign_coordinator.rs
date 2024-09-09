// Copyright (C) 2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use hashbrown::{HashMap, HashSet};
use libsigner::v0::messages::{BlockResponse, MinerSlotID, SignerMessage as SignerMessageV0};
use libsigner::v1::messages::{MessageSlotID, SignerMessage as SignerMessageV1};
use libsigner::{BlockProposal, SignerEntries, SignerEvent, SignerSession, StackerDBSession};
use stacks::burnchains::Burnchain;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::burn::{BlockSnapshot, ConsensusHash};
use stacks::chainstate::nakamoto::{NakamotoBlock, NakamotoBlockHeader, NakamotoChainState};
use stacks::chainstate::stacks::boot::{NakamotoSignerEntry, RewardSet, MINERS_NAME, SIGNERS_NAME};
use stacks::chainstate::stacks::db::StacksChainState;
use stacks::chainstate::stacks::events::StackerDBChunksEvent;
use stacks::chainstate::stacks::{Error as ChainstateError, ThresholdSignature};
use stacks::libstackerdb::StackerDBChunkData;
use stacks::net::stackerdb::StackerDBs;
use stacks::types::PublicKey;
use stacks::util::hash::MerkleHashFunc;
use stacks::util::secp256k1::MessageSignature;
use stacks::util_lib::boot::boot_code_id;
use stacks_common::bitvec::BitVec;
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::{StacksPrivateKey, StacksPublicKey};
use wsts::common::PolyCommitment;
use wsts::curve::ecdsa;
use wsts::curve::point::Point;
use wsts::curve::scalar::Scalar;
use wsts::state_machine::coordinator::fire::Coordinator as FireCoordinator;
use wsts::state_machine::coordinator::{Config as CoordinatorConfig, Coordinator};
use wsts::state_machine::PublicKeys;
use wsts::v2::Aggregator;

use super::Error as NakamotoNodeError;
use crate::event_dispatcher::STACKER_DB_CHANNEL;
use crate::neon::Counters;
use crate::Config;

/// Fault injection flag to prevent the miner from seeing enough signer signatures.
/// Used to test that the signers will broadcast a block if it gets enough signatures
#[cfg(test)]
pub static TEST_IGNORE_SIGNERS: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// How long should the coordinator poll on the event receiver before
/// waking up to check timeouts?
static EVENT_RECEIVER_POLL: Duration = Duration::from_millis(500);

/// The `SignCoordinator` struct represents a WSTS FIRE coordinator whose
///  sole function is to serve as the coordinator for Nakamoto block signing.
///  This coordinator does not operate as a DKG coordinator. Rather, this struct
///  is used by Nakamoto miners to act as the coordinator for the blocks they
///  produce.
pub struct SignCoordinator {
    coordinator: FireCoordinator<Aggregator>,
    receiver: Option<Receiver<StackerDBChunksEvent>>,
    message_key: Scalar,
    wsts_public_keys: PublicKeys,
    is_mainnet: bool,
    miners_session: StackerDBSession,
    signing_round_timeout: Duration,
    signer_entries: HashMap<u32, NakamotoSignerEntry>,
    weight_threshold: u32,
    total_weight: u32,
    pub next_signer_bitvec: BitVec<4000>,
}

pub struct NakamotoSigningParams {
    /// total number of signers
    pub num_signers: u32,
    /// total number of keys
    pub num_keys: u32,
    /// threshold of keys needed to form a valid signature
    pub threshold: u32,
    /// map of signer_id to controlled key_ids
    pub signer_key_ids: HashMap<u32, HashSet<u32>>,
    /// ECDSA public keys as Point objects indexed by signer_id
    pub signer_public_keys: HashMap<u32, Point>,
    pub wsts_public_keys: PublicKeys,
}

impl Drop for SignCoordinator {
    fn drop(&mut self) {
        STACKER_DB_CHANNEL.replace_receiver(self.receiver.take().expect(
            "FATAL: lost possession of the StackerDB channel before dropping SignCoordinator",
        ));
    }
}

impl NakamotoSigningParams {
    pub fn parse(
        is_mainnet: bool,
        reward_set: &[NakamotoSignerEntry],
    ) -> Result<Self, ChainstateError> {
        let parsed = SignerEntries::parse(is_mainnet, reward_set).map_err(|e| {
            ChainstateError::InvalidStacksBlock(format!(
                "Invalid Reward Set: Could not parse into WSTS structs: {e:?}"
            ))
        })?;

        let num_keys = parsed
            .count_keys()
            .expect("FATAL: more than u32::max() signers in the reward set");
        let num_signers = parsed
            .count_signers()
            .expect("FATAL: more than u32::max() signers in the reward set");
        let threshold = parsed
            .get_signing_threshold()
            .expect("FATAL: more than u32::max() signers in the reward set");

        Ok(NakamotoSigningParams {
            num_signers,
            threshold,
            num_keys,
            signer_key_ids: parsed.coordinator_key_ids,
            signer_public_keys: parsed.signer_public_keys,
            wsts_public_keys: parsed.public_keys,
        })
    }
}

#[allow(dead_code)]
fn get_signer_commitments(
    is_mainnet: bool,
    reward_set: &[NakamotoSignerEntry],
    stackerdbs: &StackerDBs,
    reward_cycle: u64,
    expected_aggregate_key: &Point,
) -> Result<Vec<(u32, PolyCommitment)>, ChainstateError> {
    let commitment_contract =
        MessageSlotID::DkgResults.stacker_db_contract(is_mainnet, reward_cycle);
    let signer_set_len = u32::try_from(reward_set.len())
        .map_err(|_| ChainstateError::InvalidStacksBlock("Reward set length exceeds u32".into()))?;
    for signer_id in 0..signer_set_len {
        let Some(signer_data) = stackerdbs.get_latest_chunk(&commitment_contract, signer_id)?
        else {
            warn!(
                "Failed to fetch DKG result, will look for results from other signers.";
                "signer_id" => signer_id
            );
            continue;
        };
        let Ok(SignerMessageV1::DkgResults {
            aggregate_key,
            party_polynomials,
        }) = SignerMessageV1::consensus_deserialize(&mut signer_data.as_slice())
        else {
            warn!(
                "Failed to parse DKG result, will look for results from other signers.";
                "signer_id" => signer_id,
            );
            continue;
        };

        if &aggregate_key != expected_aggregate_key {
            warn!(
                "Aggregate key in DKG results does not match expected, will look for results from other signers.";
                "expected" => %expected_aggregate_key,
                "reported" => %aggregate_key,
            );
            continue;
        }
        let computed_key = party_polynomials
            .iter()
            .fold(Point::default(), |s, (_, comm)| s + comm.poly[0]);

        if expected_aggregate_key != &computed_key {
            warn!(
                "Aggregate key computed from DKG results does not match expected, will look for results from other signers.";
                "expected" => %expected_aggregate_key,
                "computed" => %computed_key,
            );
            continue;
        }

        return Ok(party_polynomials);
    }
    error!(
        "No valid DKG results found for the active signing set, cannot coordinate a group signature";
        "reward_cycle" => reward_cycle,
    );
    Err(ChainstateError::InvalidStacksBlock(
        "Failed to fetch DKG results for the active signer set".into(),
    ))
}

impl SignCoordinator {
    /// * `reward_set` - the active reward set data, used to construct the signer
    ///    set parameters.
    /// * `message_key` - the signing key that the coordinator will use to sign messages
    ///    broadcasted to the signer set. this should be the miner's registered key.
    /// * `aggregate_public_key` - the active aggregate key for this cycle
    pub fn new(
        reward_set: &RewardSet,
        message_key: Scalar,
        config: &Config,
    ) -> Result<Self, ChainstateError> {
        let is_mainnet = config.is_mainnet();
        let Some(ref reward_set_signers) = reward_set.signers else {
            error!("Could not initialize signing coordinator for reward set without signer");
            debug!("reward set: {:?}", &reward_set);
            return Err(ChainstateError::NoRegisteredSigners(0));
        };

        let rpc_socket = config
            .node
            .get_rpc_loopback()
            .ok_or_else(|| ChainstateError::MinerAborted)?;
        let miners_contract_id = boot_code_id(MINERS_NAME, is_mainnet);
        let miners_session = StackerDBSession::new(&rpc_socket.to_string(), miners_contract_id);

        let next_signer_bitvec: BitVec<4000> = BitVec::zeros(
            reward_set_signers
                .clone()
                .len()
                .try_into()
                .expect("FATAL: signer set length greater than u16"),
        )
        .expect("FATAL: unable to construct initial bitvec for signer set");

        let NakamotoSigningParams {
            num_signers,
            num_keys,
            threshold,
            signer_key_ids,
            signer_public_keys,
            wsts_public_keys,
        } = NakamotoSigningParams::parse(is_mainnet, reward_set_signers.as_slice())?;
        debug!(
            "Initializing miner/coordinator";
            "num_signers" => num_signers,
            "num_keys" => num_keys,
            "threshold" => threshold,
            "signer_key_ids" => ?signer_key_ids,
            "signer_public_keys" => ?signer_public_keys,
            "wsts_public_keys" => ?wsts_public_keys,
        );
        let coord_config = CoordinatorConfig {
            num_signers,
            num_keys,
            threshold,
            signer_key_ids,
            signer_public_keys,
            dkg_threshold: threshold,
            message_private_key: message_key.clone(),
            ..Default::default()
        };

        let total_weight = reward_set.total_signing_weight().map_err(|e| {
            warn!("Failed to calculate total weight for the reward set: {e:?}");
            ChainstateError::NoRegisteredSigners(0)
        })?;

        let threshold = NakamotoBlockHeader::compute_voting_weight_threshold(total_weight)?;

        let signer_public_keys = reward_set_signers
            .iter()
            .cloned()
            .enumerate()
            .map(|(idx, signer)| {
                let Ok(slot_id) = u32::try_from(idx) else {
                    return Err(ChainstateError::InvalidStacksBlock(
                        "Signer index exceeds u32".into(),
                    ));
                };
                Ok((slot_id, signer))
            })
            .collect::<Result<HashMap<_, _>, ChainstateError>>()?;

        let coordinator: FireCoordinator<Aggregator> = FireCoordinator::new(coord_config);
        #[cfg(test)]
        {
            // In test mode, short-circuit spinning up the SignCoordinator if the TEST_SIGNING
            //  channel has been created. This allows integration tests for the stacks-node
            //  independent of the stacks-signer.
            use crate::tests::nakamoto_integrations::TEST_SIGNING;
            if TEST_SIGNING.lock().unwrap().is_some() {
                debug!("Short-circuiting spinning up coordinator from signer commitments. Using test signers channel.");
                let (receiver, replaced_other) = STACKER_DB_CHANNEL.register_miner_coordinator();
                if replaced_other {
                    warn!("Replaced the miner/coordinator receiver of a prior thread. Prior thread may have crashed.");
                }
                let sign_coordinator = Self {
                    coordinator,
                    message_key,
                    receiver: Some(receiver),
                    wsts_public_keys,
                    is_mainnet,
                    miners_session,
                    signing_round_timeout: config.miner.wait_on_signers.clone(),
                    next_signer_bitvec,
                    signer_entries: signer_public_keys,
                    weight_threshold: threshold,
                    total_weight,
                };
                return Ok(sign_coordinator);
            }
        }

        let (receiver, replaced_other) = STACKER_DB_CHANNEL.register_miner_coordinator();
        if replaced_other {
            warn!("Replaced the miner/coordinator receiver of a prior thread. Prior thread may have crashed.");
        }

        Ok(Self {
            coordinator,
            message_key,
            receiver: Some(receiver),
            wsts_public_keys,
            is_mainnet,
            miners_session,
            signing_round_timeout: config.miner.wait_on_signers.clone(),
            next_signer_bitvec,
            signer_entries: signer_public_keys,
            weight_threshold: threshold,
            total_weight,
        })
    }

    fn get_sign_id(burn_block_height: u64, burnchain: &Burnchain) -> u64 {
        burnchain
            .pox_constants
            .reward_cycle_index(burnchain.first_block_height, burn_block_height)
            .expect("FATAL: tried to initialize WSTS coordinator before first burn block height")
    }

    /// Send a message over the miners contract using a `Scalar` private key
    fn send_miners_message_scalar<M: StacksMessageCodec>(
        message_key: &Scalar,
        sortdb: &SortitionDB,
        tip: &BlockSnapshot,
        stackerdbs: &StackerDBs,
        message: M,
        miner_slot_id: MinerSlotID,
        is_mainnet: bool,
        miners_session: &mut StackerDBSession,
        election_sortition: &ConsensusHash,
    ) -> Result<(), String> {
        let mut miner_sk = StacksPrivateKey::from_slice(&message_key.to_bytes()).unwrap();
        miner_sk.set_compress_public(true);
        Self::send_miners_message(
            &miner_sk,
            sortdb,
            tip,
            stackerdbs,
            message,
            miner_slot_id,
            is_mainnet,
            miners_session,
            election_sortition,
        )
    }

    /// Send a message over the miners contract using a `StacksPrivateKey`
    pub fn send_miners_message<M: StacksMessageCodec>(
        miner_sk: &StacksPrivateKey,
        sortdb: &SortitionDB,
        tip: &BlockSnapshot,
        stackerdbs: &StackerDBs,
        message: M,
        miner_slot_id: MinerSlotID,
        is_mainnet: bool,
        miners_session: &mut StackerDBSession,
        election_sortition: &ConsensusHash,
    ) -> Result<(), String> {
        let Some(slot_range) = NakamotoChainState::get_miner_slot(sortdb, tip, &election_sortition)
            .map_err(|e| format!("Failed to read miner slot information: {e:?}"))?
        else {
            return Err("No slot for miner".into());
        };

        let slot_id = slot_range
            .start
            .saturating_add(miner_slot_id.to_u8().into());
        if !slot_range.contains(&slot_id) {
            return Err("Not enough slots for miner messages".into());
        }
        // Get the LAST slot version number written to the DB. If not found, use 0.
        // Add 1 to get the NEXT version number
        // Note: we already check above for the slot's existence
        let miners_contract_id = boot_code_id(MINERS_NAME, is_mainnet);
        let slot_version = stackerdbs
            .get_slot_version(&miners_contract_id, slot_id)
            .map_err(|e| format!("Failed to read slot version: {e:?}"))?
            .unwrap_or(0)
            .saturating_add(1);
        let mut chunk = StackerDBChunkData::new(slot_id, slot_version, message.serialize_to_vec());
        chunk
            .sign(&miner_sk)
            .map_err(|_| "Failed to sign StackerDB chunk")?;
        debug!("SignCoordinator: sending chunk to stackerdb: {chunk:?}");
        match miners_session.put_chunk(&chunk) {
            Ok(ack) => {
                if ack.accepted {
                    debug!("Wrote message to stackerdb: {ack:?}");
                    Ok(())
                } else {
                    warn!("Failed to write message to stackerdb: {ack:?}");
                    Err(format!("{ack:?}"))
                }
            }
            Err(e) => {
                warn!("Failed to write message to stackerdb {e:?}");
                Err(format!("{e:?}"))
            }
        }
    }

    #[cfg_attr(test, mutants::skip)]
    #[cfg(any(test, feature = "testing"))]
    pub fn begin_sign_v1(
        &mut self,
        block: &NakamotoBlock,
        burn_block_height: u64,
        block_attempt: u64,
        burn_tip: &BlockSnapshot,
        burnchain: &Burnchain,
        sortdb: &SortitionDB,
        stackerdbs: &StackerDBs,
        counters: &Counters,
        election_sortiton: &ConsensusHash,
    ) -> Result<ThresholdSignature, NakamotoNodeError> {
        let sign_id = Self::get_sign_id(burn_tip.block_height, burnchain);
        let sign_iter_id = block_attempt;
        let reward_cycle_id = burnchain
            .block_height_to_reward_cycle(burn_tip.block_height)
            .expect("FATAL: tried to initialize coordinator before first burn block height");
        self.coordinator.current_sign_id = sign_id;
        self.coordinator.current_sign_iter_id = sign_iter_id;

        let proposal_msg = BlockProposal {
            block: block.clone(),
            burn_height: burn_block_height,
            reward_cycle: reward_cycle_id,
        };

        let block_bytes = proposal_msg.serialize_to_vec();
        let nonce_req_msg = self
            .coordinator
            .start_signing_round(&block_bytes, false, None)
            .map_err(|e| {
                NakamotoNodeError::SigningCoordinatorFailure(format!(
                    "Failed to start signing round in FIRE coordinator: {e:?}"
                ))
            })?;
        Self::send_miners_message_scalar::<SignerMessageV1>(
            &self.message_key,
            sortdb,
            burn_tip,
            &stackerdbs,
            nonce_req_msg.into(),
            MinerSlotID::BlockProposal,
            self.is_mainnet,
            &mut self.miners_session,
            election_sortiton,
        )
        .map_err(NakamotoNodeError::SigningCoordinatorFailure)?;
        counters.bump_naka_proposed_blocks();
        #[cfg(test)]
        {
            // In test mode, short-circuit waiting for the signers if the TEST_SIGNING
            //  channel has been created. This allows integration tests for the stacks-node
            //  independent of the stacks-signer.
            if let Some(_signatures) =
                crate::tests::nakamoto_integrations::TestSigningChannel::get_signature()
            {
                debug!("Short-circuiting waiting for signers, using test signature");
                return Ok(ThresholdSignature::empty());
            }
        }

        let Some(ref mut receiver) = self.receiver else {
            return Err(NakamotoNodeError::SigningCoordinatorFailure(
                "Failed to obtain the StackerDB event receiver".into(),
            ));
        };

        let start_ts = Instant::now();
        while start_ts.elapsed() <= self.signing_round_timeout {
            let event = match receiver.recv_timeout(EVENT_RECEIVER_POLL) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(NakamotoNodeError::SigningCoordinatorFailure(
                        "StackerDB event receiver disconnected".into(),
                    ))
                }
            };

            let is_signer_event =
                event.contract_id.name.starts_with(SIGNERS_NAME) && event.contract_id.is_boot();
            if !is_signer_event {
                debug!("Ignoring StackerDB event for non-signer contract"; "contract" => %event.contract_id);
                continue;
            }
            let modified_slots = &event.modified_slots;

            // Update `next_signers_bitvec` with the slots that were modified in the event
            modified_slots.iter().for_each(|chunk| {
                if let Ok(slot_id) = chunk.slot_id.try_into() {
                    match &self.next_signer_bitvec.set(slot_id, true) {
                        Err(e) => {
                            warn!("Failed to set bitvec for next signer: {e:?}");
                        }
                        _ => (),
                    };
                } else {
                    error!("FATAL: slot_id greater than u16, which should never happen.");
                }
            });

            let Ok(signer_event) = SignerEvent::try_from(event).map_err(|e| {
                warn!("Failure parsing StackerDB event into signer event. Ignoring message."; "err" => ?e);
            }) else {
                continue;
            };
            let SignerEvent::SignerMessages(signer_set, messages) = signer_event else {
                debug!("Received signer event other than a signer message. Ignoring.");
                continue;
            };
            if signer_set != u32::try_from(reward_cycle_id % 2).unwrap() {
                debug!("Received signer event for other reward cycle. Ignoring.");
                continue;
            };
            debug!("Miner/Coordinator: Received messages from signers"; "count" => messages.len());
            let coordinator_pk = ecdsa::PublicKey::new(&self.message_key).map_err(|_e| {
                NakamotoNodeError::MinerSignatureError("Bad signing key for the FIRE coordinator")
            })?;
            let packets: Vec<_> = messages
                .into_iter()
                .filter_map(|msg| match msg {
                    SignerMessageV1::DkgResults { .. }
                    | SignerMessageV1::BlockResponse(_)
                    | SignerMessageV1::EncryptedSignerState(_)
                    | SignerMessageV1::Transactions(_) => None,
                    SignerMessageV1::Packet(packet) => {
                        debug!("Received signers packet: {packet:?}");
                        if !packet.verify(&self.wsts_public_keys, &coordinator_pk) {
                            warn!("Failed to verify StackerDB packet: {packet:?}");
                            None
                        } else {
                            Some(packet)
                        }
                    }
                })
                .collect();
            let (outbound_msgs, op_results) = self
                .coordinator
                .process_inbound_messages(&packets)
                .unwrap_or_else(|e| {
                    error!(
                        "Miner/Coordinator: Failed to process inbound message packets";
                        "err" => ?e
                    );
                    (vec![], vec![])
                });
            for operation_result in op_results.into_iter() {
                match operation_result {
                    wsts::state_machine::OperationResult::Dkg { .. }
                    | wsts::state_machine::OperationResult::SignTaproot(_)
                    | wsts::state_machine::OperationResult::DkgError(_) => {
                        debug!("Ignoring unrelated operation result");
                    }
                    wsts::state_machine::OperationResult::Sign(signature) => {
                        // check if the signature actually corresponds to our block?
                        let block_sighash = block.header.signer_signature_hash();
                        let verified = signature.verify(
                            self.coordinator.aggregate_public_key.as_ref().unwrap(),
                            &block_sighash.0,
                        );
                        let signature = ThresholdSignature(signature);
                        if !verified {
                            warn!(
                                "Processed signature but didn't validate over the expected block. Returning error.";
                                "signature" => %signature,
                                "block_signer_signature_hash" => %block_sighash
                            );
                            return Err(NakamotoNodeError::SignerSignatureError(
                                "Signature failed to validate over the expected block".into(),
                            ));
                        } else {
                            info!(
                                "SignCoordinator: Generated a valid signature for the block";
                                "next_signer_bitvec" => self.next_signer_bitvec.binary_str(),
                            );
                            return Ok(signature);
                        }
                    }
                    wsts::state_machine::OperationResult::SignError(e) => {
                        return Err(NakamotoNodeError::SignerSignatureError(format!(
                            "Signing failed: {e:?}"
                        )))
                    }
                }
            }
            for msg in outbound_msgs {
                match Self::send_miners_message_scalar::<SignerMessageV1>(
                    &self.message_key,
                    sortdb,
                    burn_tip,
                    stackerdbs,
                    msg.into(),
                    // TODO: note, in v1, we'll want to add a new slot, but for now, it just shares
                    //   with the block proposal
                    MinerSlotID::BlockProposal,
                    self.is_mainnet,
                    &mut self.miners_session,
                    election_sortiton,
                ) {
                    Ok(()) => {
                        debug!("Miner/Coordinator: sent outbound message.");
                    }
                    Err(e) => {
                        warn!(
                            "Miner/Coordinator: Failed to send message to StackerDB instance: {e:?}."
                        );
                    }
                };
            }
        }

        Err(NakamotoNodeError::SignerSignatureError(
            "Timed out waiting for group signature".into(),
        ))
    }

    /// Do we ignore signer signatures?
    #[cfg(test)]
    fn fault_injection_ignore_signatures() -> bool {
        if *TEST_IGNORE_SIGNERS.lock().unwrap() == Some(true) {
            return true;
        }
        false
    }

    #[cfg(not(test))]
    fn fault_injection_ignore_signatures() -> bool {
        false
    }

    /// Start gathering signatures for a Nakamoto block.
    /// This function begins by sending a `BlockProposal` message
    /// to the signers, and then waits for the signers to respond
    /// with their signatures.  It does so in two ways, concurrently:
    /// * It waits for signer StackerDB messages with signatures. If enough signatures can be
    /// found, then the block can be broadcast.
    /// * It waits for the chainstate to contain the relayed block. If so, then its signatures are
    /// loaded and returned. This can happen if the node receives the block via a signer who
    /// fetched all signatures and assembled the signature vector, all before we could.
    // Mutants skip here: this function is covered via integration tests,
    //  which the mutation testing does not see.
    #[cfg_attr(test, mutants::skip)]
    pub fn run_sign_v0(
        &mut self,
        block: &NakamotoBlock,
        block_attempt: u64,
        burn_tip: &BlockSnapshot,
        burnchain: &Burnchain,
        sortdb: &SortitionDB,
        chain_state: &mut StacksChainState,
        stackerdbs: &StackerDBs,
        counters: &Counters,
        election_sortition: &ConsensusHash,
    ) -> Result<Vec<MessageSignature>, NakamotoNodeError> {
        let sign_id = Self::get_sign_id(burn_tip.block_height, burnchain);
        let sign_iter_id = block_attempt;
        let reward_cycle_id = burnchain
            .block_height_to_reward_cycle(burn_tip.block_height)
            .expect("FATAL: tried to initialize coordinator before first burn block height");
        self.coordinator.current_sign_id = sign_id;
        self.coordinator.current_sign_iter_id = sign_iter_id;

        let block_proposal = BlockProposal {
            block: block.clone(),
            burn_height: burn_tip.block_height,
            reward_cycle: reward_cycle_id,
        };

        let block_proposal_message = SignerMessageV0::BlockProposal(block_proposal);
        debug!("Sending block proposal message to signers";
            "signer_signature_hash" => %block.header.signer_signature_hash(),
        );
        Self::send_miners_message_scalar::<SignerMessageV0>(
            &self.message_key,
            sortdb,
            burn_tip,
            &stackerdbs,
            block_proposal_message,
            MinerSlotID::BlockProposal,
            self.is_mainnet,
            &mut self.miners_session,
            election_sortition,
        )
        .map_err(NakamotoNodeError::SigningCoordinatorFailure)?;
        counters.bump_naka_proposed_blocks();

        #[cfg(test)]
        {
            info!(
                "SignCoordinator: sent block proposal to .miners, waiting for test signing channel"
            );
            // In test mode, short-circuit waiting for the signers if the TEST_SIGNING
            //  channel has been created. This allows integration tests for the stacks-node
            //  independent of the stacks-signer.
            if let Some(signatures) =
                crate::tests::nakamoto_integrations::TestSigningChannel::get_signature()
            {
                debug!("Short-circuiting waiting for signers, using test signature");
                return Ok(signatures);
            }
        }

        let Some(ref mut receiver) = self.receiver else {
            return Err(NakamotoNodeError::SigningCoordinatorFailure(
                "Failed to obtain the StackerDB event receiver".into(),
            ));
        };

        let mut total_weight_signed: u32 = 0;
        let mut total_reject_weight: u32 = 0;
        let mut gathered_signatures = BTreeMap::new();

        info!("SignCoordinator: beginning to watch for block signatures OR posted blocks.";
            "threshold" => self.weight_threshold,
        );

        let start_ts = Instant::now();
        while start_ts.elapsed() <= self.signing_round_timeout {
            // look in the nakamoto staging db -- a block can only get stored there if it has
            // enough signing weight to clear the threshold
            if let Ok(Some((stored_block, _sz))) = chain_state
                .nakamoto_blocks_db()
                .get_nakamoto_block(&block.block_id())
                .map_err(|e| {
                    warn!(
                        "Failed to query chainstate for block {}: {:?}",
                        &block.block_id(),
                        &e
                    );
                    e
                })
            {
                debug!("SignCoordinator: Found signatures in relayed block");
                counters.bump_naka_signer_pushed_blocks();
                return Ok(stored_block.header.signer_signature);
            }

            // one of two things can happen:
            // * we get enough signatures from stackerdb from the signers, OR
            // * we see our block get processed in our chainstate (meaning, the signers broadcasted
            // the block and our node got it and processed it)
            let event = match receiver.recv_timeout(EVENT_RECEIVER_POLL) {
                Ok(event) => event,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(NakamotoNodeError::SigningCoordinatorFailure(
                        "StackerDB event receiver disconnected".into(),
                    ))
                }
            };

            // check to see if this event we got is a signer event
            let is_signer_event =
                event.contract_id.name.starts_with(SIGNERS_NAME) && event.contract_id.is_boot();

            if !is_signer_event {
                debug!("Ignoring StackerDB event for non-signer contract"; "contract" => %event.contract_id);
                continue;
            }

            let modified_slots = &event.modified_slots.clone();

            let Ok(signer_event) = SignerEvent::<SignerMessageV0>::try_from(event).map_err(|e| {
                warn!("Failure parsing StackerDB event into signer event. Ignoring message."; "err" => ?e);
            }) else {
                continue;
            };
            let SignerEvent::SignerMessages(signer_set, messages) = signer_event else {
                debug!("Received signer event other than a signer message. Ignoring.");
                continue;
            };
            if signer_set != u32::try_from(reward_cycle_id % 2).unwrap() {
                debug!("Received signer event for other reward cycle. Ignoring.");
                continue;
            };
            let slot_ids = modified_slots
                .iter()
                .map(|chunk| chunk.slot_id)
                .collect::<Vec<_>>();

            debug!("SignCoordinator: Received messages from signers";
                "count" => messages.len(),
                "slot_ids" => ?slot_ids,
                "threshold" => self.weight_threshold
            );

            for (message, slot_id) in messages.into_iter().zip(slot_ids) {
                let (response_hash, signature) = match message {
                    SignerMessageV0::BlockResponse(BlockResponse::Accepted((
                        response_hash,
                        signature,
                    ))) => (response_hash, signature),
                    SignerMessageV0::BlockResponse(BlockResponse::Rejected(rejected_data)) => {
                        let Some(signer_entry) = &self.signer_entries.get(&slot_id) else {
                            return Err(NakamotoNodeError::SignerSignatureError(
                                "Signer entry not found".into(),
                            ));
                        };
                        if rejected_data.signer_signature_hash
                            != block.header.signer_signature_hash()
                        {
                            debug!("Received rejected block response for a block besides my own. Ignoring.");
                            continue;
                        }

                        debug!(
                            "Signer {} rejected our block {}/{}",
                            slot_id,
                            &block.header.consensus_hash,
                            &block.header.block_hash()
                        );
                        total_reject_weight = total_reject_weight
                            .checked_add(signer_entry.weight)
                            .expect("FATAL: total weight rejected exceeds u32::MAX");

                        if total_reject_weight.saturating_add(self.weight_threshold)
                            > self.total_weight
                        {
                            debug!(
                                "{}/{} signers vote to reject our block {}/{}",
                                total_reject_weight,
                                self.total_weight,
                                &block.header.consensus_hash,
                                &block.header.block_hash()
                            );
                            counters.bump_naka_rejected_blocks();
                            return Err(NakamotoNodeError::SignersRejected);
                        }
                        continue;
                    }
                    SignerMessageV0::BlockProposal(_) => {
                        debug!("Received block proposal message. Ignoring.");
                        continue;
                    }
                    SignerMessageV0::BlockPushed(_) => {
                        debug!("Received block pushed message. Ignoring.");
                        continue;
                    }
                    SignerMessageV0::MockSignature(_)
                    | SignerMessageV0::MockProposal(_)
                    | SignerMessageV0::MockBlock(_) => {
                        debug!("Received mock message. Ignoring.");
                        continue;
                    }
                };
                let block_sighash = block.header.signer_signature_hash();
                if block_sighash != response_hash {
                    warn!(
                        "Processed signature for a different block. Will try to continue.";
                        "signature" => %signature,
                        "block_signer_signature_hash" => %block_sighash,
                        "response_hash" => %response_hash,
                        "slot_id" => slot_id,
                        "reward_cycle_id" => reward_cycle_id,
                        "response_hash" => %response_hash
                    );
                    continue;
                }
                debug!("SignCoordinator: Received valid signature from signer"; "slot_id" => slot_id, "signature" => %signature);
                let Some(signer_entry) = &self.signer_entries.get(&slot_id) else {
                    return Err(NakamotoNodeError::SignerSignatureError(
                        "Signer entry not found".into(),
                    ));
                };
                let Ok(signer_pubkey) = StacksPublicKey::from_slice(&signer_entry.signing_key)
                else {
                    return Err(NakamotoNodeError::SignerSignatureError(
                        "Failed to parse signer public key".into(),
                    ));
                };
                let Ok(valid_sig) = signer_pubkey.verify(block_sighash.bits(), &signature) else {
                    warn!("Got invalid signature from a signer. Ignoring.");
                    continue;
                };
                if !valid_sig {
                    warn!(
                        "Processed signature but didn't validate over the expected block. Ignoring";
                        "signature" => %signature,
                        "block_signer_signature_hash" => %block_sighash,
                        "slot_id" => slot_id,
                    );
                    continue;
                }
                if !gathered_signatures.contains_key(&slot_id) {
                    total_weight_signed = total_weight_signed
                        .checked_add(signer_entry.weight)
                        .expect("FATAL: total weight signed exceeds u32::MAX");
                }

                if Self::fault_injection_ignore_signatures() {
                    warn!("SignCoordinator: fault injection: ignoring well-formed signature for block";
                        "block_signer_sighash" => %block_sighash,
                        "signer_pubkey" => signer_pubkey.to_hex(),
                        "signer_slot_id" => slot_id,
                        "signature" => %signature,
                        "signer_weight" => signer_entry.weight,
                        "total_weight_signed" => total_weight_signed,
                        "stacks_block_hash" => %block.header.block_hash(),
                        "stacks_block_id" => %block.header.block_id()
                    );
                    continue;
                }

                info!("SignCoordinator: Signature Added to block";
                    "block_signer_sighash" => %block_sighash,
                    "signer_pubkey" => signer_pubkey.to_hex(),
                    "signer_slot_id" => slot_id,
                    "signature" => %signature,
                    "signer_weight" => signer_entry.weight,
                    "total_weight_signed" => total_weight_signed,
                    "stacks_block_hash" => %block.header.block_hash(),
                    "stacks_block_id" => %block.header.block_id()
                );
                gathered_signatures.insert(slot_id, signature);
            }

            // After gathering all signatures, return them if we've hit the threshold
            if total_weight_signed >= self.weight_threshold {
                info!("SignCoordinator: Received enough signatures. Continuing.";
                    "stacks_block_hash" => %block.header.block_hash(),
                    "stacks_block_id" => %block.header.block_id()
                );
                return Ok(gathered_signatures.values().cloned().collect());
            }
        }

        Err(NakamotoNodeError::SignerSignatureError(
            "Timed out waiting for group signature".into(),
        ))
    }
}
