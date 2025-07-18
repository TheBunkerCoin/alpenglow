// Copyright (c) Anza Technology, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Core consensus logic and data structures.
//!
//! The central structure of the consensus protocol is [`Alpenglow`].
//! It contains all state for a single consensus instance and also has access
//! to the different necessary network protocols.
//!
//! Most important component data structures defined in this module are:
//! - [`Blockstore`] holds individual shreds and reconstructed blocks for each slot.
//! - [`Pool`] holds votes and certificates for each slot.
//! - [`Votor`] handles the main voting logic.
//!
//! Some other data types for consensus are also defined here:
//! - [`Cert`] represents a certificate of votes of a specific type.
//! - [`Vote`] represents a vote of a specific type.
//! - [`EpochInfo`] holds information about the epoch and all validators.

mod blockstore;
mod cert;
mod epoch_info;
mod pool;
mod vote;
mod votor;

use std::marker::{Send, Sync};
use std::time::Instant;
use std::{sync::Arc, time::Duration};

use color_eyre::Result;
use fastrace::Span;
use fastrace::future::FutureExt;
use log::{debug, info, trace, warn};
use rand::rngs::SmallRng;
use rand::{RngCore, SeedableRng};
use tokio::sync::{RwLock, mpsc};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::crypto::{Hash, aggsig, signature};
use crate::network::{Network, NetworkError, NetworkMessage};
use crate::repair::{Repair, RepairMessage};
use crate::shredder::{MAX_DATA_PER_SLICE, RegularShredder, Shred, Shredder, Slice};
use crate::shredder;
use crate::{All2All, Disseminator, Slot, ValidatorInfo};

pub use blockstore::{BlockInfo, Blockstore};
pub use blockstore::BlockMetadata;
pub use cert::Cert;
pub use epoch_info::EpochInfo;
pub use pool::{Pool, PoolError};
pub use vote::Vote;
use votor::{Votor, VotorEvent};

/// Number of slots in each leader window.
pub const SLOTS_PER_WINDOW: u64 = 2;
/// Number of slots in each epoch.
pub const SLOTS_PER_EPOCH: u64 = 4_500;
/// Time bound assumed on network transmission delays during periods of synchrony.
const DELTA: Duration = Duration::from_millis(8_000);
/// Target time for block production (slot length)
const TARGET_BLOCK_TIME: Duration = Duration::from_millis(60_000);
/// Time the leader has for producing and sending the block.
const DELTA_BLOCK: Duration = Duration::from_millis(120_000);
/// Timeout to use when we haven't seen any shred from the leader's block.
const DELTA_EARLY_TIMEOUT: Duration = Duration::from_millis(180_000);
// const DELTA_EARLY_TIMEOUT: Duration = DELTA.checked_mul(2).unwrap();
/// Timeout to use when we have seen at least one shred from the leader's block.
const DELTA_TIMEOUT: Duration = Duration::from_millis(240_000);
// const DELTA_TIMEOUT: Duration = DELTA_EARLY_TIMEOUT.checked_add(DELTA_BLOCK).unwrap();
/// Timeout for standstill detection mechanism.
const DELTA_STANDSTILL: Duration = Duration::from_millis(300_000);



/// Alpenglow consensus protocol implementation.
pub struct Alpenglow<A: All2All, D: Disseminator, R: Network> {
    /// Own validator's secret key (used e.g. for block production).
    /// This is not the same as the voting secret key, which is held by [`Votor`].
    secret_key: signature::SecretKey,
    /// Other validators' info.
    epoch_info: Arc<EpochInfo>,

    /// Blockstore for storing raw block data.
    blockstore: Arc<RwLock<Blockstore>>,
    /// Pool of votes and certificates.
    pool: Arc<RwLock<Pool>>,

    /// All-to-all broadcast network protocol for consensus messages.
    all2all: Arc<A>,
    /// Block dissemination network protocol for shreds.
    disseminator: Arc<D>,
    /// Block repair protocol.
    repair: Arc<Repair<R>>,

    /// Indicates whether the node is shutting down.
    cancel_token: CancellationToken,
    /// Votor task handle.
    votor_handle: tokio::task::JoinHandle<()>,
}

impl<A, D, R> Alpenglow<A, D, R>
where
    A: All2All + Sync + Send + 'static,
    D: Disseminator + Sync + Send + 'static,
    R: Network + Sync + Send + 'static,
{
    /// Creates a new Alpenglow consensus node.
    #[must_use]
    pub fn new(
        secret_key: signature::SecretKey,
        voting_secret_key: aggsig::SecretKey,
        all2all: A,
        disseminator: D,
        repair_network: R,
        epoch_info: Arc<EpochInfo>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let (votor_tx, votor_rx) = mpsc::channel(1024);
        let (repair_tx, mut repair_rx) = mpsc::channel(1024);
        let all2all = Arc::new(all2all);

        let blockstore = Blockstore::new(epoch_info.clone(), votor_tx.clone());
        let blockstore = Arc::new(RwLock::new(blockstore));
        let mut pool = Pool::new(epoch_info.clone(), votor_tx.clone(), repair_tx.clone());
        pool.set_blockstore(Arc::clone(&blockstore));
        let pool = Arc::new(RwLock::new(pool));
        let repair = Repair::new(
            Arc::clone(&blockstore),
            Arc::clone(&pool),
            repair_network,
            epoch_info.clone(),
        );
        let repair = Arc::new(repair);

        let r = Arc::clone(&repair);
        let _repair_handle = tokio::spawn(
            async move {
                while let Some((slot, hash)) = repair_rx.recv().await {
                    r.repair_block(slot, hash).await;
                }
            }
            .in_span(Span::enter_with_local_parent("repair loop")),
        );

        // let cancel = cancel_token.clone();
        let mut votor = Votor::new(
            epoch_info.own_id,
            voting_secret_key,
            votor_tx.clone(),
            votor_rx,
            all2all.clone(),
            repair_tx,
        );
        let votor_handle = tokio::spawn(
            async move { votor.voting_loop().await.unwrap() }
                .in_span(Span::enter_with_local_parent("voting loop")),
        );

        Self {
            secret_key,
            epoch_info,
            blockstore,
            pool,
            all2all,
            disseminator: Arc::new(disseminator),
            repair,
            cancel_token,
            votor_handle,
        }
    }

    /// Starts the different tasks of the Alpenglow node.
    ///
    /// # Errors
    ///
    /// Returns an error only if any of the tasks panics.
    #[fastrace::trace(short_name = true)]
    pub async fn run(self) -> Result<()> {
        // clean load after startup
        {
            let pool_guard = self.pool.read().await;
            let highest_finalized = pool_guard.finalized_slot();
            drop(pool_guard);
            
            let mut blockstore_guard = self.blockstore.write().await;
            blockstore_guard.clean_beyond_finalized(highest_finalized);
            drop(blockstore_guard);
        }

        let msg_loop_span = Span::enter_with_local_parent("message loop");
        let node = Arc::new(self);
        let nn = node.clone();
        let msg_loop = tokio::spawn(async move { nn.message_loop().await }.in_span(msg_loop_span));

        let standstill_loop_span = Span::enter_with_local_parent("standstill detection loop");
        let nn = node.clone();
        let standstill_loop =
            tokio::spawn(async move { nn.standstill_loop().await }.in_span(standstill_loop_span));

        let block_production_span = Span::enter_with_local_parent("block production");
        let nn = node.clone();
        let prod_loop = tokio::spawn(
            async move { nn.block_production_loop().await }.in_span(block_production_span),
        );

        node.cancel_token.cancelled().await;
        node.votor_handle.abort();
        msg_loop.abort();
        standstill_loop.abort();
        prod_loop.abort();

        let (msg_res, prod_res) = tokio::join!(msg_loop, prod_loop);
        msg_res??;
        prod_res??;
        Ok(())
    }

    pub fn get_info(&self) -> &ValidatorInfo {
        self.epoch_info.validator(self.epoch_info.own_id)
    }

    pub fn get_pool(&self) -> Arc<RwLock<Pool>> {
        Arc::clone(&self.pool)
    }

    pub fn get_cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Handles incoming messages on all the different network interfaces.
    ///
    /// [`All2All`]: Handles incoming votes and certificates. Adds them to the [`Pool`].
    /// [`Disseminator`]: Handles incoming shreds. Adds them to the [`Blockstore`].
    /// [`Repair`]: Answers incoming repair requests.
    async fn message_loop(self: &Arc<Self>) -> Result<()> {
        loop {
            tokio::select! {
                // handle incoming votes and certificates
                res = self.all2all.receive() => self.handle_all2all_message(res?).await?,
                // handle shreds received by block dissemination protocol
                res = self.disseminator.receive() => self.handle_disseminator_shred(res?).await?,
                // handle repair requests
                res = self.repair.receive() => self.handle_repair_message(res?).await?,

                () = self.cancel_token.cancelled() => return Ok(()),
            };
        }
    }

    /// Handles standstill detection and triggers recovery.
    ///
    /// Keeps track of when consensus progresses, i.e., [`Pool`] finalizes new blocks.
    /// Triggers standstill recovery if no progress was detected for a long time.
    async fn standstill_loop(self: &Arc<Self>) -> Result<()> {
        let mut finalized_slot = 0;
        let mut last_progress = Instant::now();
        loop {
            let slot = self.pool.read().await.finalized_slot();
            if slot > finalized_slot {
                finalized_slot = slot;
                last_progress = Instant::now();
            } else if last_progress.elapsed() > DELTA_STANDSTILL {
                self.pool.read().await.recover_from_standstill().await;
                last_progress = Instant::now();
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
    }

    /// Handles the leader side of the consensus protocol.
    ///
    /// Once all previous blocks have been notarized or skipped and the next
    /// slot belongs to our leader window, we will produce a block.
    async fn block_production_loop(&self) -> Result<()> {
        let mut parent: Slot = 0;
        let mut parent_hash = Hash::default();
        let mut parent_ready = true;

        'outer: for window in 0.. {
            if self.cancel_token.is_cancelled() {
                break;
            }

            let first_slot_in_window = window * SLOTS_PER_WINDOW;
            let last_slot_in_window = (window + 1) * SLOTS_PER_WINDOW - 1;

            // don't do anything if we are not the leader
            let leader = self.epoch_info.leader(first_slot_in_window);
            if leader.id != self.epoch_info.own_id {
                continue;
            }

            if self.pool.read().await.finalized_slot() >= last_slot_in_window {
                warn!(
                    "ignoring window {}..{} for block production",
                    first_slot_in_window, last_slot_in_window
                );
                continue;
            }

            // wait for potential parent of first slot (except if first window)
            if window > 0 {
                // PERF: maybe replace busy loop with events
                (parent, parent_hash, parent_ready) = loop {
                    // build on ready parent, if any
                    let pool = self.pool.read().await;
                    if let Some((slot, hash)) = pool.parents_ready(first_slot_in_window).first() {
                        let h = &hex::encode(hash)[..8];
                        debug!("building block on ready parent {h} in slot {slot}");
                        break (*slot, *hash, true);
                    }
                    drop(pool);

                    // optimisitically build on block in previous slot, if any
                    let blockstore = self.blockstore.read().await;
                    if let Some(hash) = blockstore.canonical_block_hash(first_slot_in_window - 1) {
                        let slot = first_slot_in_window - 1;
                        let h = &hex::encode(hash)[..8];
                        debug!("optimistically building block on parent {h} in slot {slot}",);
                        break (slot, hash, false);
                    }
                    drop(blockstore);

                    if self.pool.read().await.finalized_slot() >= last_slot_in_window {
                        warn!(
                            "ignoring window {}..{} for block production",
                            first_slot_in_window, last_slot_in_window
                        );
                        continue 'outer;
                    }

                    sleep(Duration::from_millis(1)).await;
                };
            }

            // produce blocks for all slots in window
            let mut block = parent;
            let mut block_hash = parent_hash;
            for slot in first_slot_in_window..=last_slot_in_window {
                self.produce_block(slot, (block, block_hash), parent_ready)
                    .await?;
                parent_ready = true;

                // build off own block next
                let blockstore = self.blockstore.read().await;
                block = slot;
                block_hash = blockstore.canonical_block_hash(slot).unwrap();
            }
        }

        Ok(())
    }

    async fn produce_block(
        &self,
        slot: Slot,
        parent: (Slot, Hash),
        parent_ready: bool,
    ) -> Result<()> {
        let (parent_slot, parent_hash) = parent;
        let _slot_span = Span::enter_with_local_parent(format!("slot {slot}"));
        let mut rng = SmallRng::seed_from_u64(slot);
        let ph = &hex::encode(parent_hash)[..8];
        info!("producing block in slot {slot} with parent {ph} in slot {parent_slot}",);

        for slice_index in 0..1 {
            let start_time = Instant::now();
            let min_len = 48;
            let padded_len = ((min_len + shredder::DATA_SHREDS - 1) / shredder::DATA_SHREDS) * shredder::DATA_SHREDS;
            let mut data = vec![0u8; padded_len]; // pad to next multiple of DATA_SHREDS
            // pack parent information in first slice
            if slice_index == 0 {
                data[0..8].copy_from_slice(&parent_slot.to_be_bytes());
                data[8..40].copy_from_slice(&parent_hash);
            }
            let slice = Slice {
                slot,
                slice_index,
                is_last: slice_index == 0,
                merkle_root: None,
                data,
            };

            // shred and disseminate slice
            let shreds = RegularShredder::shred(&slice, &self.secret_key).unwrap();
            for s in shreds {
                self.disseminator.send(&s).await?;
                // PERF: move expensive add_shred() call out of block production
                let mut blockstore = self.blockstore.write().await;
                let block = blockstore.add_shred(s, true).await;
                if let Some((slot, block_info)) = block {
                    let mut pool = self.pool.write().await;
                    pool.add_block(slot, block_info).await;
                }
            }

            // switch parent if necessary (for optimistic handover)
            if !parent_ready {
                let pool = self.pool.read().await;
                if let Some(p) = pool.parents_ready(slot).first() {
                    if *p != parent {
                        warn!("switching block production parent");
                        unimplemented!("have to switch parents");
                    }
                }
            }

            // artificially ensure block time close to target (400ms in good conditions)
            sleep(TARGET_BLOCK_TIME.saturating_sub(start_time.elapsed())).await;
        }

        Ok(())
    }

    #[fastrace::trace(short_name = true)]
    async fn handle_all2all_message(&self, msg: NetworkMessage) -> Result<(), NetworkError> {
        trace!("received all2all msg: {msg:?}");
        match msg {
            NetworkMessage::Vote(v) => match self.pool.write().await.add_vote(v).await {
                Ok(()) => {}
                Err(PoolError::Slashable(offence)) => {
                    warn!("slashable offence detected: {offence}");
                }
                Err(err) => trace!("ignoring invalid vote: {err}"),
            },
            NetworkMessage::Cert(c) => match self.pool.write().await.add_cert(c).await {
                Ok(()) => {}
                Err(err) => trace!("ignoring invalid cert: {err}"),
            },
            msg => warn!("unexpected message on all2all port: {msg:?}"),
        }
        Ok(())
    }

    #[fastrace::trace(short_name = true)]
    async fn handle_disseminator_shred(&self, shred: Shred) -> Result<(), NetworkError> {
        self.disseminator.forward(&shred).await?;
        let b = self.blockstore.write().await.add_shred(shred, true).await;
        if let Some((slot, block_info)) = b {
            let mut guard = self.pool.write().await;
            guard.add_block(slot, block_info).await;
        }
        Ok(())
    }

    async fn handle_repair_message(&self, msg: RepairMessage) -> Result<(), NetworkError> {
        match msg {
            RepairMessage::Request(request) => {
                self.repair.answer_request(request).await?;
            }
            RepairMessage::Response(resposne) => {
                self.repair.handle_response(resposne).await;
            }
        }
        Ok(())
    }

    pub fn blockstore(&self) -> std::sync::Arc<tokio::sync::RwLock<crate::consensus::Blockstore>> {
        std::sync::Arc::clone(&self.blockstore)
    }
}
