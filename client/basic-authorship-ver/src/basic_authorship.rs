// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! A consensus proposer for "basic" chains which use the primitive inherent-data.

// FIXME #1021 move this into sp-consensus

#[cfg(doc)]
use aquamarine::aquamarine;
use codec::Encode;
use futures::{
	channel::oneshot,
	future,
	future::{Future, FutureExt},
	select,
};
use log::{debug, error, info, trace, warn};
use sc_block_builder::{validate_transaction, BlockBuilderApi, BlockBuilderProvider};
use sc_client_api::backend;
use sc_telemetry::{telemetry, TelemetryHandle, CONSENSUS_INFO};
use sc_transaction_pool_api::{InPoolTransaction, TransactionPool};
use sp_api::{ApiExt, ProvideRuntimeApi};
use sp_blockchain::{ApplyExtrinsicFailed::Validity, Error::ApplyExtrinsicFailed, HeaderBackend};
use sp_consensus::{DisableProofRecording, EnableProofRecording, ProofRecording, Proposal};
use sp_core::{traits::SpawnNamed, ShufflingSeed};
use sp_inherents::InherentData;
use sp_runtime::{
	traits::{BlakeTwo256, Block as BlockT, Hash as HashT, Header as HeaderT},
	Digest, Percent, SaturatedConversion,
};
use std::{marker::PhantomData, pin::Pin, sync::Arc, time};
use ver_api::VerApi;

use prometheus_endpoint::Registry as PrometheusRegistry;
use sc_proposer_metrics::{EndProposingReason, MetricsLink as PrometheusMetrics};

/// Default block size limit in bytes used by [`Proposer`].
///
/// Can be overwritten by [`ProposerFactory::set_default_block_size_limit`].
///
/// Be aware that there is also an upper packet size on what the networking code
/// will accept. If the block doesn't fit in such a package, it can not be
/// transferred to other nodes.
pub const DEFAULT_BLOCK_SIZE_LIMIT: usize = 4 * 1024 * 1024 + 512;

const DEFAULT_SOFT_DEADLINE_PERCENT: Percent = Percent::from_percent(50);

const LOG_TARGET: &'static str = "basic-authorship";

/// [`Proposer`] factory.
pub struct ProposerFactory<A, B, C, PR> {
	spawn_handle: Box<dyn SpawnNamed>,
	/// The client instance.
	client: Arc<C>,
	/// The transaction pool.
	transaction_pool: Arc<A>,
	/// Prometheus Link,
	metrics: PrometheusMetrics,
	/// The default block size limit.
	///
	/// If no `block_size_limit` is passed to [`sp_consensus::Proposer::propose`], this block size
	/// limit will be used.
	default_block_size_limit: usize,
	/// Soft deadline percentage of hard deadline.
	///
	/// The value is used to compute soft deadline during block production.
	/// The soft deadline indicates where we should stop attempting to add transactions
	/// to the block, which exhaust resources. After soft deadline is reached,
	/// we switch to a fixed-amount mode, in which after we see `MAX_SKIPPED_TRANSACTIONS`
	/// transactions which exhaust resrouces, we will conclude that the block is full.
	soft_deadline_percent: Percent,
	telemetry: Option<TelemetryHandle>,
	/// When estimating the block size, should the proof be included?
	include_proof_in_block_size_estimation: bool,
	/// phantom member to pin the `Backend`/`ProofRecording` type.
	_phantom: PhantomData<(B, PR)>,
}

impl<A, B, C> ProposerFactory<A, B, C, DisableProofRecording> {
	/// Create a new proposer factory.
	///
	/// Proof recording will be disabled when using proposers built by this instance to build
	/// blocks.
	pub fn new(
		spawn_handle: impl SpawnNamed + 'static,
		client: Arc<C>,
		transaction_pool: Arc<A>,
		prometheus: Option<&PrometheusRegistry>,
		telemetry: Option<TelemetryHandle>,
	) -> Self {
		ProposerFactory {
			spawn_handle: Box::new(spawn_handle),
			transaction_pool,
			metrics: PrometheusMetrics::new(prometheus),
			default_block_size_limit: DEFAULT_BLOCK_SIZE_LIMIT,
			soft_deadline_percent: DEFAULT_SOFT_DEADLINE_PERCENT,
			telemetry,
			client,
			include_proof_in_block_size_estimation: false,
			_phantom: PhantomData,
		}
	}
}

impl<A, B, C> ProposerFactory<A, B, C, EnableProofRecording> {
	/// Create a new proposer factory with proof recording enabled.
	///
	/// Each proposer created by this instance will record a proof while building a block.
	///
	/// This will also include the proof into the estimation of the block size. This can be disabled
	/// by calling [`ProposerFactory::disable_proof_in_block_size_estimation`].
	pub fn with_proof_recording(
		spawn_handle: impl SpawnNamed + 'static,
		client: Arc<C>,
		transaction_pool: Arc<A>,
		prometheus: Option<&PrometheusRegistry>,
		telemetry: Option<TelemetryHandle>,
	) -> Self {
		ProposerFactory {
			client,
			spawn_handle: Box::new(spawn_handle),
			transaction_pool,
			metrics: PrometheusMetrics::new(prometheus),
			default_block_size_limit: DEFAULT_BLOCK_SIZE_LIMIT,
			soft_deadline_percent: DEFAULT_SOFT_DEADLINE_PERCENT,
			telemetry,
			include_proof_in_block_size_estimation: true,
			_phantom: PhantomData,
		}
	}

	/// Disable the proof inclusion when estimating the block size.
	pub fn disable_proof_in_block_size_estimation(&mut self) {
		self.include_proof_in_block_size_estimation = false;
	}
}

impl<A, B, C, PR> ProposerFactory<A, B, C, PR> {
	/// Set the default block size limit in bytes.
	///
	/// The default value for the block size limit is:
	/// [`DEFAULT_BLOCK_SIZE_LIMIT`].
	///
	/// If there is no block size limit passed to [`sp_consensus::Proposer::propose`], this value
	/// will be used.
	pub fn set_default_block_size_limit(&mut self, limit: usize) {
		self.default_block_size_limit = limit;
	}

	/// Set soft deadline percentage.
	///
	/// The value is used to compute soft deadline during block production.
	/// The soft deadline indicates where we should stop attempting to add transactions
	/// to the block, which exhaust resources. After soft deadline is reached,
	/// we switch to a fixed-amount mode, in which after we see `MAX_SKIPPED_TRANSACTIONS`
	/// transactions which exhaust resrouces, we will conclude that the block is full.
	///
	/// Setting the value too low will significantly limit the amount of transactions
	/// we try in case they exhaust resources. Setting the value too high can
	/// potentially open a DoS vector, where many "exhaust resources" transactions
	/// are being tried with no success, hence block producer ends up creating an empty block.
	pub fn set_soft_deadline(&mut self, percent: Percent) {
		self.soft_deadline_percent = percent;
	}
}

impl<B, Block, C, A, PR> ProposerFactory<A, B, C, PR>
where
	A: TransactionPool<Block = Block> + 'static,
	B: backend::Backend<Block> + Send + Sync + 'static,
	Block: BlockT,
	C: BlockBuilderProvider<B, Block, C>
		+ HeaderBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ Sync
		+ 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block>,
{
	fn init_with_now(
		&mut self,
		parent_header: &<Block as BlockT>::Header,
		now: Box<dyn Fn() -> time::Instant + Send + Sync>,
	) -> Proposer<B, Block, C, A, PR> {
		let parent_hash = parent_header.hash();

		info!("🙌 Starting consensus session on top of parent {:?}", parent_hash);

		let proposer = Proposer::<_, _, _, _, PR> {
			spawn_handle: self.spawn_handle.clone(),
			client: self.client.clone(),
			parent_hash,
			parent_number: *parent_header.number(),
			transaction_pool: self.transaction_pool.clone(),
			now,
			metrics: self.metrics.clone(),
			default_block_size_limit: self.default_block_size_limit,
			soft_deadline_percent: self.soft_deadline_percent,
			telemetry: self.telemetry.clone(),
			_phantom: PhantomData,
			include_proof_in_block_size_estimation: self.include_proof_in_block_size_estimation,
		};

		proposer
	}
}

impl<A, B, Block, C, PR> sp_consensus::Environment<Block> for ProposerFactory<A, B, C, PR>
where
	A: TransactionPool<Block = Block> + 'static,
	B: backend::Backend<Block> + Send + Sync + 'static,
	Block: BlockT,
	C: BlockBuilderProvider<B, Block, C>
		+ HeaderBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ Sync
		+ 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block> + VerApi<Block>,
	PR: ProofRecording,
{
	type CreateProposer = future::Ready<Result<Self::Proposer, Self::Error>>;
	type Proposer = Proposer<B, Block, C, A, PR>;
	type Error = sp_blockchain::Error;

	fn init(&mut self, parent_header: &<Block as BlockT>::Header) -> Self::CreateProposer {
		future::ready(Ok(self.init_with_now(parent_header, Box::new(time::Instant::now))))
	}
}

/// The proposer logic.
pub struct Proposer<B, Block: BlockT, C, A: TransactionPool, PR> {
	spawn_handle: Box<dyn SpawnNamed>,
	client: Arc<C>,
	parent_hash: Block::Hash,
	parent_number: <<Block as BlockT>::Header as HeaderT>::Number,
	transaction_pool: Arc<A>,
	now: Box<dyn Fn() -> time::Instant + Send + Sync>,
	metrics: PrometheusMetrics,
	default_block_size_limit: usize,
	include_proof_in_block_size_estimation: bool,
	soft_deadline_percent: Percent,
	telemetry: Option<TelemetryHandle>,
	_phantom: PhantomData<(B, PR)>,
}

impl<A, B, Block, C, PR> sp_consensus::Proposer<Block> for Proposer<B, Block, C, A, PR>
where
	A: TransactionPool<Block = Block> + 'static,
	B: backend::Backend<Block> + Send + Sync + 'static,
	Block: BlockT,
	C: BlockBuilderProvider<B, Block, C>
		+ HeaderBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ Sync
		+ 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block> + VerApi<Block>,
	PR: ProofRecording,
{
	type Proposal =
		Pin<Box<dyn Future<Output = Result<Proposal<Block, PR::Proof>, Self::Error>> + Send>>;
	type Error = sp_blockchain::Error;
	type ProofRecording = PR;
	type Proof = PR::Proof;

	#[cfg_attr(doc, aquamarine)]
	/// This function is responsible for block creation. [`Proposer`] is tightly coupled with
	/// [`sc_block_builder::BlockBuilder`] that wraps "lower level" aspects of block creation where
	/// [`Proposer`] main responsibility is keeping track of block limits (weight, size, execution
	/// time).
	///
	/// Block limits are:
	/// - X weight
	/// - Y execution time
	/// - Z block size in bytes
	///
	/// Lets call these limits a "slot". [`Proposer`] divides that slot into 2 halves resulting with
	/// two smaller slots, where each has limits:
	/// - X/2 weight
	/// - Y/2 execution time
	/// - Z/2 block size in bytes
	///
	/// First of the 'smaller slots' is used for executing txs that were included in previous
	/// blocks. Txs are fetched from the storage queue that is stored in blockchain runtime storage.
	///
	/// Second 'smaller slot' is used for fetching txs from transaction pool. If tx is validated
	/// successfully it is stored into storage queue.
	///
	/// [`Proposer`] splits that limits into half and uses first
	/// ```mermaid
	/// sequenceDiagram
	///    participant TransactionPool
	///    Proposer->>BlockBuilder: create
	///    BlockBuilder->>RuntimeApi: initialize_block
	///    BlockBuilder->>Proposer: instance
	///    Proposer->>BlockBuilder: create_inherents
	///    BlockBuilder->>BlockBuilder: extract seed from inherent data
	///    BlockBuilder->>RuntimeApi: inherent_extrinsics
	///    RuntimeApi->>BlockBuilder: inherents
	///    BlockBuilder->>Proposer: (seed,inherents)
	///    Proposer->>BlockBuilder: apply_previous_block_extrinsics
	///    BlockBuilder->>RuntimeApi: store seed
	///    RuntimeApi->>FrameSystem: shuffle txs stored in previous block(N-1)
	///
	///    loop while half of size/weight/exec time limit is not exceeded
	///        Note over FrameSystem: ideally all txs from previous block should be consumed
	///        BlockBuilder->>FrameSystem: fetch tx from storage queue
	///        FrameSystem->>BlockBuilder: ready tx
	///        BlockBuilder->>BlockBuilder: execute tx
	///    end
	///
	///
	///    Proposer->>Proposer: initialize list of valid txs: VALID_TXS
	///    loop while second half of size/weight/exec time limit is not exceeded
	///        Proposer->>TransactionPool: fetch ready tx
	///        TransactionPool->>Proposer: ready tx
	///        Proposer->>Proposer: validate txs
	///        alt tx is valid
	///            Proposer->>Proposer: VALID_TXS.push(tx)
	///        else
	///            Proposer->>Proposer: reject tx
	///        end
	///    end
	///
	///    Proposer->>BlockBuilder: build_block_with_seed
	///    BlockBuilder->>RuntimeApi: create_enqueue_txs_inherent(VALID_TXS)
	///    RuntimeApi->>BlockBuilder: inhernet
	///    BlockBuilder->>RuntimeApi: apply_extrinsic(inhernet)
	///    RuntimeApi->>FrameSystem: store txs into storage queue
	///    BlockBuilder->>RuntimeApi: finalize_block
	///    RuntimeApi->>BlockBuilder: Header
	///    BlockBuilder->>Proposer: block
	/// ```
	fn propose(
		self,
		inherent_data: InherentData,
		inherent_digests: Digest,
		max_duration: time::Duration,
		block_size_limit: Option<usize>,
	) -> Self::Proposal {
		let (tx, rx) = oneshot::channel();
		let spawn_handle = self.spawn_handle.clone();

		spawn_handle.spawn_blocking(
			"basic-authorship-proposer",
			None,
			Box::pin(async move {
				// leave some time for evaluation and block finalization (33%)
				let deadline = (self.now)() + max_duration - max_duration / 3;
				let res = self
					.propose_with(inherent_data, inherent_digests, deadline, block_size_limit)
					.await;
				if tx.send(res).is_err() {
					trace!(
						target: LOG_TARGET,
						"Could not send block production result to proposer!"
					);
				}
			}),
		);

		async move { rx.await? }.boxed()
	}
}

/// If the block is full we will attempt to push at most
/// this number of transactions before quitting for real.
/// It allows us to increase block utilization.
const MAX_SKIPPED_TRANSACTIONS: usize = 8;

impl<A, B, Block, C, PR> Proposer<B, Block, C, A, PR>
where
	A: TransactionPool<Block = Block>,
	B: backend::Backend<Block> + Send + Sync + 'static,
	Block: BlockT,
	C: BlockBuilderProvider<B, Block, C>
		+ HeaderBackend<Block>
		+ ProvideRuntimeApi<Block>
		+ Send
		+ Sync
		+ 'static,
	C::Api: ApiExt<Block> + BlockBuilderApi<Block> + VerApi<Block>,
	PR: ProofRecording,
{
	async fn propose_with(
		self,
		inherent_data: InherentData,
		inherent_digests: Digest,
		deadline: time::Instant,
		block_size_limit: Option<usize>,
	) -> Result<Proposal<Block, PR::Proof>, sp_blockchain::Error> {
		let propose_with_timer = time::Instant::now();
		let mut block_builder =
			self.client.new_block_at(self.parent_hash, inherent_digests, PR::ENABLED)?;

		let seed = self.apply_inherents(&mut block_builder, inherent_data)?;

		// TODO call `after_inherents` and check if we should apply extrinsincs here
		// <https://github.com/paritytech/substrate/pull/14275/>

		let block_timer = time::Instant::now();

		// apply_extrinsics
		// proceed with transactions
		// We calculate soft deadline used only in case we start skipping transactions.
		let now = (self.now)();
		let left = deadline.saturating_duration_since(now);
		let left_micros: u64 = left.as_micros().saturated_into();
		let first_slot_limit =
			futures_timer::Delay::new(time::Duration::from_micros(left_micros * 55 / 100));

		// let queue_processing_deadline = now + time::Duration::from_micros(left_micros / 2);
		let queue_processing_deadline = now + time::Duration::from_micros(left_micros * 55 / 100);

		let mut skipped = 0;
		let mut unqueue_invalid = Vec::new();

		let mut t1 = self.transaction_pool.ready_at(self.parent_number).fuse();

		let mut block_size = 0;

		let get_current_time = &self.now;
		let is_expired = || get_current_time() > queue_processing_deadline;

		let block_size_limit = block_size_limit.unwrap_or(self.default_block_size_limit);
		block_builder.apply_previous_block_extrinsics(
			seed.clone(),
			&mut block_size,
			block_size_limit / 2, // txs from queue should not occupy more than half of the block
			is_expired,
		);

		// there might be some txs comming in that time - so its better to sleep than
		// shortening remaining time
		debug!(target: LOG_TARGET, "sleeping by the end of the slot");
		first_slot_limit.await;

		// artificially simulate that block is half filled
		// also include header & proof cost for the second part to make sure
		// that all txs included in that phase will have enought room to be executed in following
		// block
		debug!(target: LOG_TARGET, "esitmated block size{}", block_builder.estimate_block_size_without_extrinsics(self.include_proof_in_block_size_estimation));
		block_size = block_size_limit / 2 +
			block_builder.estimate_block_size_without_extrinsics(
				self.include_proof_in_block_size_estimation,
			);

		let now = (self.now)();

		let mut t2 =
			futures_timer::Delay::new(deadline.saturating_duration_since((self.now)()) / 8).fuse();

		let mut pending_iterator = select! {
			res = t1 => res,
			_ = t2 => {
				warn!(target: LOG_TARGET,
					"Timeout fired waiting for transaction pool at block #{}. \
					Proceeding with production.",
					self.parent_number,
				);
				self.transaction_pool.ready()
			},
		};

		debug!(target: LOG_TARGET, "Attempting to push transactions from the pool.");
		debug!(target: LOG_TARGET, "Pool status: {:?}", self.transaction_pool.status());
		let mut transaction_pushed = false;
		let mut end_reason = EndProposingReason::NoMoreTransactions;

		let left = deadline.saturating_duration_since(now);
		let left_micros: u64 = left.as_micros().saturated_into();

		let soft_deadline =
			now + time::Duration::from_micros(self.soft_deadline_percent.mul_floor(left_micros));

		// after previous block is applied it is possible to prevalidate incomming transaction
		// but eventually changess needs to be rolled back, as those can be executed
		// only in the following(future) block
		let (block, storage_changes, proof) = block_builder
			.build_with_seed(seed, |at, api| {
				let mut valid_txs = Vec::new();

				end_reason = loop {
					let pending_tx = if let Some(pending_tx) = pending_iterator.next() {
						pending_tx
					} else {
						break EndProposingReason::NoMoreTransactions;
					};

					let now = (self.now)();
					if now > deadline {
						debug!(
							target: LOG_TARGET,
							"Consensus deadline reached when pushing block transactions, \
							proceeding with proposing."
						);
						break EndProposingReason::HitDeadline;
					}

					let pending_tx_data = pending_tx.data().clone();
					let pending_tx_hash = pending_tx.hash().clone();

					block_size += pending_tx_data.encoded_size();
					block_size += sp_core::H256::len_bytes();

					if block_size > block_size_limit {
						pending_iterator.report_invalid(&pending_tx);
						if skipped < MAX_SKIPPED_TRANSACTIONS {
							skipped += 1;
							debug!(
								target: LOG_TARGET,
								"Transaction would overflow the block size limit, \
								 but will try {} more transactions before quitting.",
								MAX_SKIPPED_TRANSACTIONS - skipped,
							);
							continue;
						} else if now < soft_deadline {
							debug!(
								target: LOG_TARGET,
								"Transaction would overflow the block size limit, \
								 but we still have time before the soft deadline, so \
								 we will try a bit more."
							);
							continue;
						} else {
							debug!(
								target: LOG_TARGET,
								"Reached block size limit, proceeding with proposing."
							);
							break EndProposingReason::HitBlockSizeLimit;
						}
					}

					trace!(target: LOG_TARGET, "[{:?}] Pushing to the block.", pending_tx_hash);
					let who = api
						.get_signer(*at, pending_tx_data.clone())
						.unwrap()
						.map(|signer_info| signer_info.0.clone());
					match validate_transaction::<Block, C>(*at, &api, pending_tx_data.clone()) {
						Ok(()) => {
							transaction_pushed = true;
							valid_txs.push((who, pending_tx_data));
							debug!(target: LOG_TARGET, "[{:?}] Pushed to the block.", pending_tx_hash);
						},
						Err(ApplyExtrinsicFailed(Validity(e))) if e.exhausted_resources() => {
							pending_iterator.report_invalid(&pending_tx);
							if skipped < MAX_SKIPPED_TRANSACTIONS {
								skipped += 1;
								debug!(target: LOG_TARGET,
									"Block seems full, but will try {} more transactions before quitting.",
									MAX_SKIPPED_TRANSACTIONS - skipped,
								);
							} else if (self.now)() < soft_deadline {
								debug!(target: LOG_TARGET,
									"Block seems full, but we still have time before the soft deadline, \
									 so we will try a bit more before quitting."
								);
							} else {
								debug!(target: LOG_TARGET, "now {:?}", (self.now)());
								trace!(target: LOG_TARGET, "soft_deadline : {:?}", soft_deadline.saturating_duration_since(now).as_secs_f64());
								debug!(
									target: LOG_TARGET,
									"Reached block weight limit, proceeding with proposing."
								);
								break EndProposingReason::HitBlockWeightLimit;
							}
						},
						Err(e) if skipped > 0 => {
							pending_iterator.report_invalid(&pending_tx);
							trace!(
								target: LOG_TARGET,
								"[{:?}] Ignoring invalid transaction when skipping: {}",
								pending_tx_hash,
								e
							);
						},
						Err(e) => {
							pending_iterator.report_invalid(&pending_tx);
							debug!(
								target: LOG_TARGET,
								"[{:?}] Invalid transaction: {}", pending_tx_hash, e
							);
							unqueue_invalid.push(pending_tx_hash);
						},
					}
				};
				valid_txs
			})?
			.into_inner();

		if matches!(end_reason, EndProposingReason::HitBlockSizeLimit) && !transaction_pushed {
			warn!(
				target: LOG_TARGET,
				"Hit block size limit of `{}` without including any transaction!", block_size_limit,
			);
		}

		self.transaction_pool.remove_invalid(&unqueue_invalid);
		// end apply_extrinsics

		let block_took = block_timer.elapsed();
		debug!(target: LOG_TARGET,"created block {:?}", block);
		debug!(target: LOG_TARGET,"created block with hash {}", block.header().hash());

		let proof =
			PR::into_proof(proof).map_err(|e| sp_blockchain::Error::Application(Box::new(e)))?;

		self.print_summary(&block, end_reason, block_took, propose_with_timer.elapsed());
		Ok(Proposal { block, proof, storage_changes })
	}

	/// Apply all inherents to the block.
	fn apply_inherents(
		&self,
		block_builder: &mut sc_block_builder::BlockBuilder<'_, Block, C, B>,
		inherent_data: InherentData,
	) -> Result<ShufflingSeed, sp_blockchain::Error> {
		let create_inherents_start = time::Instant::now();
		let (seed, inherents) = block_builder.create_inherents(inherent_data.clone())?;
		let create_inherents_end = time::Instant::now();

		self.metrics.report(|metrics| {
			metrics.create_inherents_time.observe(
				create_inherents_end
					.saturating_duration_since(create_inherents_start)
					.as_secs_f64(),
			);
		});

		debug!(target: LOG_TARGET, "found {} inherents", inherents.len());
		for inherent in inherents {
			debug!(target: LOG_TARGET, "processing inherent");
			// TODO now it actually commits changes
			match block_builder.push(inherent) {
				Err(ApplyExtrinsicFailed(Validity(e))) if e.exhausted_resources() => {
					warn!(
						target: LOG_TARGET,
						"⚠️  Dropping non-mandatory inherent from overweight block."
					)
				},
				Err(ApplyExtrinsicFailed(Validity(e))) if e.was_mandatory() => {
					error!(
						"❌️ Mandatory inherent extrinsic returned error. Block cannot be produced."
					);
					return Err(ApplyExtrinsicFailed(Validity(e)))
				},
				Err(e) => {
					warn!(
						target: LOG_TARGET,
						"❗️ Inherent extrinsic returned unexpected error: {}. Dropping.", e
					);
				},
				Ok(_) => {
					trace!(target:LOG_TARGET, "inherent pushed into the block");
				},
			}
		}
		Ok(seed)
	}

	/// Prints a summary and does telemetry + metrics.
	fn print_summary(
		&self,
		block: &Block,
		end_reason: EndProposingReason,
		block_took: time::Duration,
		propose_with_took: time::Duration,
	) {
		let extrinsics = block.extrinsics();
		self.metrics.report(|metrics| {
			metrics.number_of_transactions.set(extrinsics.len() as u64);
			metrics.block_constructed.observe(block_took.as_secs_f64());
			metrics.report_end_proposing_reason(end_reason);
			metrics.create_block_proposal_time.observe(propose_with_took.as_secs_f64());
		});

		let extrinsics_summary = if extrinsics.is_empty() {
			"no extrinsics".to_string()
		} else {
			format!(
				"extrinsics ({}): [{}]",
				extrinsics.len(),
				extrinsics
					.iter()
					.map(|xt| BlakeTwo256::hash_of(xt).to_string())
					.collect::<Vec<_>>()
					.join(", ")
			)
		};

		info!(
			"🎁 Prepared block for proposing at {} ({} ms) [hash: {:?}; parent_hash: {}; {extrinsics_summary}",
			block.header().number(),
			block_took.as_millis(),
			<Block as BlockT>::Hash::from(block.header().hash()),
			block.header().parent_hash(),
		);
		telemetry!(
			self.telemetry;
			CONSENSUS_INFO;
			"prepared_block_for_proposing";
			"number" => ?block.header().number(),
			"hash" => ?<Block as BlockT>::Hash::from(block.header().hash()),
		);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use futures::executor::block_on;
	use parking_lot::Mutex;
	use sc_client_api::Backend;
	use sc_transaction_pool::BasicPool;
	use sc_transaction_pool_api::{ChainEvent, MaintainedTransactionPool, TransactionSource};
	use sp_api::Core;
	use sp_blockchain::HeaderBackend;
	use sp_consensus::{Environment, Proposer};
	use sp_core::Pair;
	use sp_inherents::InherentDataProvider;
	use sp_runtime::{
		generic::{BlockId, UncheckedExtrinsic},
		traits::NumberFor,
		Perbill,
	};
	use substrate_test_runtime_client::{
		prelude::*,
		runtime::{
			substrate_test_pallet::pallet::Call as PalletCall, Block as TestBlock, Extrinsic,
			ExtrinsicBuilder, RuntimeCall, Transfer,
		},
		TestClientBuilder, TestClientBuilderExt,
	};

	const SOURCE: TransactionSource = TransactionSource::External;

	// Note:
	// Maximum normal extrinsic size for `substrate_test_runtime` is ~65% of max_block (refer to
	// `substrate_test_runtime::RuntimeBlockWeights` for details).
	// This extrinsic sizing allows for:
	// - one huge xts + a lot of tiny dust
	// - one huge, no medium,
	// - two medium xts
	// This is widely exploited in following tests.
	const HUGE: u32 = 649000000;
	const MEDIUM: u32 = 250000000;
	const TINY: u32 = 1000;

	fn extrinsic(nonce: u64) -> Extrinsic {
		ExtrinsicBuilder::new_fill_block(Perbill::from_parts(TINY)).nonce(nonce).build()
	}

	fn chain_event<B: BlockT>(header: B::Header) -> ChainEvent<B>
	where
		NumberFor<B>: From<u64>,
	{
		ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None }
	}

	#[tokio::test]
	async fn should_cease_building_block_when_deadline_is_reached() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		);

		block_on(txpool.submit_at(&BlockId::number(0), SOURCE, vec![extrinsic(0), extrinsic(1)]))
			.unwrap();

		block_on(
			txpool.maintain(chain_event(
				client
					.expect_header(client.info().genesis_hash)
					.expect("there should be header"),
			)),
		);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let cell = Mutex::new((false, time::Instant::now()));
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(client.info().genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				if !value.0 {
					value.0 = true;
					return value.1
				}
				let old = value.1;
				let new = old + time::Duration::from_secs(1);
				*value = (true, new);
				old
			}),
		);

		// when
		let deadline = time::Duration::from_secs(3);

		let mut inherent_data = InherentData::new();
		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.await
				.unwrap();
		}

		let block = block_on(proposer.propose(inherent_data, Default::default(), deadline, None))
			.map(|r| r.block)
			.unwrap();

		// then
		// block should have some extrinsics although we have some more in the pool.
		assert_eq!(block.extrinsics().len(), 1);
		assert_eq!(txpool.ready().count(), 2);
	}

	#[tokio::test]
	async fn should_not_panic_when_deadline_is_reached() {
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let cell = Mutex::new((false, time::Instant::now()));
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(client.info().genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				if !value.0 {
					value.0 = true;
					return value.1
				}
				let new = value.1 + time::Duration::from_secs(160);
				*value = (true, new);
				new
			}),
		);

		let mut inherent_data = InherentData::new();
		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.await
				.unwrap();
		}

		let deadline = time::Duration::from_secs(1);
		block_on(proposer.propose(inherent_data, Default::default(), deadline, None))
			.map(|r| r.block)
			.unwrap();
	}

	#[tokio::test]
	async fn proposed_storage_changes_should_match_execute_block_storage_changes() {
		let (client, _) = TestClientBuilder::new().build_with_backend();
		let client = Arc::new(client);
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		);

		let genesis_hash = client.info().best_hash;

		block_on(txpool.submit_at(&BlockId::number(0), SOURCE, vec![extrinsic(0)])).unwrap();

		block_on(
			txpool.maintain(chain_event(
				client
					.expect_header(client.info().genesis_hash)
					.expect("there should be header"),
			)),
		);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let proposer = proposer_factory.init_with_now(
			&client.header(genesis_hash).unwrap().unwrap(),
			Box::new(move || time::Instant::now()),
		);

		let mut inherent_data = InherentData::new();
		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.await
				.unwrap();
		}

		let deadline = time::Duration::from_secs(9);
		let proposal =
			block_on(proposer.propose(inherent_data, Default::default(), deadline, None)).unwrap();

		assert_eq!(proposal.block.extrinsics().len(), 1);

		// as test runtime does not implement ver block execution below does not apply
		// let api = client.runtime_api();
		// api.execute_block(&block_id, proposal.block).unwrap();
		//
		// let state = backend.state_at(block_id).unwrap();
		//
		// let storage_changes = api.into_storage_changes(&state, genesis_hash).unwrap();
		//
		// assert_eq!(
		// 	proposal.storage_changes.transaction_storage_root,
		// 	storage_changes.transaction_storage_root,
		// );
	}

	#[tokio::test]
	async fn should_cease_building_block_when_block_limit_is_reached() {
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		);
		let genesis_header = client
			.expect_header(client.info().genesis_hash)
			.expect("there should be header");

		let extrinsics_num = 4;
		let extrinsics = (0..extrinsics_num).map(extrinsic).collect::<Vec<_>>();

		let init_size = genesis_header.encoded_size() +
			Vec::<Extrinsic>::new().encoded_size() +
			ExtrinsicBuilder::new_enqueue(extrinsics_num).build().encoded_size();

		let block_limit = init_size +
			extrinsics
				.iter()
				.take((extrinsics_num - 1) as usize)
				.map(|tx| Encode::encoded_size(tx) + sp_core::H256::len_bytes())
				.sum::<usize>();
		block_on(txpool.submit_at(&BlockId::number(0), SOURCE, extrinsics.clone())).unwrap();

		block_on(txpool.maintain(chain_event(genesis_header.clone())));

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();

		let mut inherent_data = InherentData::new();
		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.await
				.unwrap();
		}

		// Give it enough time
		let deadline = time::Duration::from_secs(20);
		let block = block_on(proposer.propose(
			inherent_data.clone(),
			Default::default(),
			deadline,
			Some(block_limit * 2),
		))
		.map(|r| r.block)
		.unwrap();

		// Based on the block limit, one transaction shouldn't be included.
		assert_eq!(block.extrinsics().len(), 1);
		assert!(matches!(
			block.extrinsics().get(0).expect("enqueue tx extrinsic"),
			UncheckedExtrinsic { signature, function: RuntimeCall::SubstrateTest(PalletCall::enqueue { count }) } if *count == extrinsics_num - 1
		));

		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();

		let block =
			block_on(proposer.propose(inherent_data.clone(), Default::default(), deadline, None))
				.map(|r| r.block)
				.unwrap();

		// Without a block limit we should include all of them
		assert_eq!(block.extrinsics().len(), 1);
		assert!(matches!(
			block.extrinsics().get(0).expect("enqueue tx extrinsic"),
			UncheckedExtrinsic { signature, function: RuntimeCall::SubstrateTest(PalletCall::enqueue { count }) } if *count == extrinsics_num
		));

		let mut proposer_factory = ProposerFactory::with_proof_recording(
			spawner.clone(),
			client.clone(),
			txpool.clone(),
			None,
			None,
		);

		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();

		// EDIT: for some reason proof size is set to 0 in test-runtime
		// so below does not apply anymore
		//
		// Give it enough time
		// let block = block_on(proposer.propose(
		// 	Default::default(),
		// 	Default::default(),
		// 	deadline,
		// 	Some(block_limit * 2),
		// ))
		// .map(|r| r.block)
		// .unwrap();
		// The block limit didn't changed, but we now include the proof in the estimation of the
		// block size and thus, one less transaction should fit into the limit.
		// assert_eq!(block.extrinsics().len(), 1);
		// assert!(
		// 	matches!(
		// 		block.extrinsics().get(0).expect("enqueue tx extrinsic"),
		// 		Extrinsic::EnqueueTxs(count) if *count == (extrinsics_num - 2) as u64)
		// 	);
	}

	#[tokio::test]
	async fn should_keep_adding_transactions_after_exhausts_resources_before_soft_deadline() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		);

		let tiny = |nonce| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(TINY)).nonce(nonce).build()
		};
		let huge = |who| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(HUGE))
				.signer(AccountKeyring::numeric(who))
				.build()
		};

		block_on(
			txpool.submit_at(
				&BlockId::number(0),
				SOURCE,
				// add 2 * MAX_SKIPPED_TRANSACTIONS that exhaust resources
				(0..MAX_SKIPPED_TRANSACTIONS * 2)
					.into_iter()
					.map(huge)
					// and some transactions that are okay.
					.chain((0..MAX_SKIPPED_TRANSACTIONS as u64).into_iter().map(tiny))
					.collect(),
			),
		)
		.unwrap();

		block_on(
			txpool.maintain(chain_event(
				client
					.expect_header(client.info().genesis_hash)
					.expect("there should be header"),
			)),
		);
		assert_eq!(txpool.ready().count(), MAX_SKIPPED_TRANSACTIONS * 3);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let cell = Mutex::new(time::Instant::now());
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(client.info().genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				let old = *value;
				*value = old + time::Duration::from_secs(1);
				old
			}),
		);

		let mut inherent_data = InherentData::new();
		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.await
				.unwrap();
		}

		// when
		// give it enough time so that deadline is never triggered.
		let deadline = time::Duration::from_secs(90);
		let block = block_on(proposer.propose(inherent_data, Default::default(), deadline, None))
			.map(|r| r.block)
			.unwrap();

		// then block should have all non-exhaust resources extrinsics (+ the first one).
		assert_eq!(block.extrinsics().len(), 1);
		assert!(matches!(
			block.extrinsics().get(0).expect("enqueue tx extrinsic"),
			UncheckedExtrinsic { signature, function: RuntimeCall::SubstrateTest(PalletCall::enqueue { count }) } if *count == MAX_SKIPPED_TRANSACTIONS as u64 + 1
		));
	}

	#[tokio::test]
	async fn should_only_skip_up_to_some_limit_after_soft_deadline() {
		// given
		let client = Arc::new(substrate_test_runtime_client::new());
		let spawner = sp_core::testing::TaskExecutor::new();
		let txpool = BasicPool::new_full(
			Default::default(),
			true.into(),
			None,
			spawner.clone(),
			client.clone(),
		);

		let tiny = |who| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(TINY))
				.signer(AccountKeyring::numeric(who))
				.nonce(1)
				.build()
		};
		let huge = |who| {
			ExtrinsicBuilder::new_fill_block(Perbill::from_parts(HUGE))
				.signer(AccountKeyring::numeric(who))
				.build()
		};

		block_on(
			txpool.submit_at(
				&BlockId::number(0),
				SOURCE,
				(0..MAX_SKIPPED_TRANSACTIONS + 2)
					.into_iter()
					.map(huge)
					// and some transactions that are okay.
					.chain((0..MAX_SKIPPED_TRANSACTIONS).into_iter().map(tiny))
					.collect(),
			),
		)
		.unwrap();

		block_on(
			txpool.maintain(chain_event(
				client
					.expect_header(client.info().genesis_hash)
					.expect("there should be header"),
			)),
		);
		assert_eq!(txpool.ready().count(), MAX_SKIPPED_TRANSACTIONS * 2 + 2);

		let mut proposer_factory =
			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);

		let deadline = time::Duration::from_secs(600);
		let cell = Arc::new(Mutex::new((0, time::Instant::now())));
		let cell2 = cell.clone();
		let proposer = proposer_factory.init_with_now(
			&client.expect_header(client.info().genesis_hash).unwrap(),
			Box::new(move || {
				let mut value = cell.lock();
				let (called, old) = *value;
				// add time after deadline is calculated internally (hence 1)
				let increase = if called == 1 {
					// we start after the soft_deadline should have already been reached.
					deadline / 2
				} else {
					// but we make sure to never reach the actual deadline
					time::Duration::from_millis(0)
				};
				*value = (called + 1, old + increase);
				old
			}),
		);

		let mut inherent_data = InherentData::new();
		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.await
				.unwrap();
		}

		let block = block_on(proposer.propose(inherent_data, Default::default(), deadline, None))
			.map(|r| r.block)
			.unwrap();

		// then the block should have no transactions despite some in the pool
		assert_eq!(block.extrinsics().len(), 1);
		assert!(
			cell2.lock().0 > MAX_SKIPPED_TRANSACTIONS,
			"Not enough calls to current time, which indicates the test might have ended because of deadline, not soft deadline"
		);
	}
}
