// This file is part of Substrate.

// Copyright (C) 2018-2021 Parity Technologies (UK) Ltd.
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

use codec::{Decode, Encode};
use ver_api::VerApi;
use sp_encrypted_tx::EncryptedTxApi;
use std::ops::Deref;
use futures::{
	channel::oneshot,
	future,
	future::{Future, FutureExt},
	select,
};
use log::{debug, error, info, trace, warn};
use sc_block_builder::{BlockBuilderApi, BlockBuilderProvider};
use sc_client_api::backend;
use sp_keystore::SyncCryptoStorePtr;
use sc_telemetry::{telemetry, TelemetryHandle, CONSENSUS_INFO};
use sc_transaction_pool_api::{InPoolTransaction, TransactionPool};
use sp_api::{ApiExt, ProvideRuntimeApi, TransactionOutcome};
use sp_blockchain::{ApplyExtrinsicFailed::Validity, Error::ApplyExtrinsicFailed, HeaderBackend};
use sp_consensus::{
	evaluation, DisableProofRecording, EnableProofRecording, ProofRecording, Proposal,
};
use sp_runtime::traits::DigestItemFor;
use sp_consensus_aura::digests::CompatibleDigestItem;
use sp_core::traits::SpawnNamed;
use sp_core::crypto::Pair;
use sp_inherents::InherentData;
use sp_runtime::{
    AccountId32,
    KeyTypeId,
	generic::BlockId,
	traits::{BlakeTwo256, Block as BlockT, DigestFor, Hash as HashT, Header as HeaderT},
};
use std::{marker::PhantomData, pin::Pin, sync::Arc, time};

use prometheus_endpoint::Registry as PrometheusRegistry;
use sc_proposer_metrics::MetricsLink as PrometheusMetrics;
use sp_inherents::InherentDataProvider;
use sp_runtime::traits::One;
use ecies::{utils::{aes_decrypt, decapsulate}, SecretKey, PublicKey};
use std::ops::Add;

/// Default block size limit in bytes used by [`Proposer`].
///
/// Can be overwritten by [`ProposerFactory::set_default_block_size_limit`].
///
/// Be aware that there is also an upper packet size on what the networking code
/// will accept. If the block doesn't fit in such a package, it can not be
/// transferred to other nodes.
pub const DEFAULT_BLOCK_SIZE_LIMIT: usize = 4 * 1024 * 1024 + 512;

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
	telemetry: Option<TelemetryHandle>,
	/// When estimating the block size, should the proof be included?
	include_proof_in_block_size_estimation: bool,
	/// phantom member to pin the `Backend`/`ProofRecording` type.
	_phantom: PhantomData<(B, PR)>,
	/// keystore for encrypted transactions
	keystore: SyncCryptoStorePtr,
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
		keystore: SyncCryptoStorePtr,
	) -> Self {
		ProposerFactory {
			spawn_handle: Box::new(spawn_handle),
			transaction_pool,
			metrics: PrometheusMetrics::new(prometheus),
			default_block_size_limit: DEFAULT_BLOCK_SIZE_LIMIT,
			telemetry,
			client,
			include_proof_in_block_size_estimation: false,
			_phantom: PhantomData,
			keystore: keystore.clone(),
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
		keystore: SyncCryptoStorePtr,
	) -> Self {
		ProposerFactory {
			client,
			spawn_handle: Box::new(spawn_handle),
			transaction_pool,
			metrics: PrometheusMetrics::new(prometheus),
			default_block_size_limit: DEFAULT_BLOCK_SIZE_LIMIT,
			telemetry,
			include_proof_in_block_size_estimation: true,
			_phantom: PhantomData,
			keystore: keystore.clone()
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
	C::Api:
		ApiExt<Block, StateBackend = backend::StateBackendFor<B, Block>> + BlockBuilderApi<Block>,
{
	fn init_with_now(
		&mut self,
		parent_header: &<Block as BlockT>::Header,
		now: Box<dyn Fn() -> time::Instant + Send + Sync>,
	) -> Proposer<B, Block, C, A, PR> {
		let parent_hash = parent_header.hash();

		let id = BlockId::hash(parent_hash);

		info!("🙌 Starting consensus session on top of parent {:?}", parent_hash);

		let proposer = Proposer::<_, _, _, _, PR> {
			spawn_handle: self.spawn_handle.clone(),
			client: self.client.clone(),
			parent_hash,
			parent_id: id,
			parent_number: *parent_header.number(),
			transaction_pool: self.transaction_pool.clone(),
			now,
			metrics: self.metrics.clone(),
			default_block_size_limit: self.default_block_size_limit,
			telemetry: self.telemetry.clone(),
			_phantom: PhantomData,
			include_proof_in_block_size_estimation: self.include_proof_in_block_size_estimation,
			keystore: self.keystore.clone(),
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
	C::Api: ApiExt<Block, StateBackend = backend::StateBackendFor<B, Block>>
		+ BlockBuilderApi<Block>
		+ EncryptedTxApi<Block>
		+ VerApi<Block>,
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
	parent_hash: <Block as BlockT>::Hash,
	parent_id: BlockId<Block>,
	parent_number: <<Block as BlockT>::Header as HeaderT>::Number,
	transaction_pool: Arc<A>,
	now: Box<dyn Fn() -> time::Instant + Send + Sync>,
	metrics: PrometheusMetrics,
	default_block_size_limit: usize,
	include_proof_in_block_size_estimation: bool,
	telemetry: Option<TelemetryHandle>,
	keystore: SyncCryptoStorePtr,
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
	C::Api: ApiExt<Block, StateBackend = backend::StateBackendFor<B, Block>>
		+ BlockBuilderApi<Block>
		+ EncryptedTxApi<Block>
		+ VerApi<Block>,
	PR: ProofRecording,
{
	type Transaction = backend::TransactionFor<B, Block>;
	type Proposal = Pin<
		Box<
			dyn Future<Output = Result<Proposal<Block, Self::Transaction, PR::Proof>, Self::Error>>
				+ Send,
		>,
	>;
	type Error = sp_blockchain::Error;
	type ProofRecording = PR;
	type Proof = PR::Proof;

	fn propose(
		self,
		mut inherent_data: InherentData,
		inherent_digests: DigestFor<Block>,
		max_duration: time::Duration,
		block_size_limit: Option<usize>,
	) -> Self::Proposal {
		let (tx, rx) = oneshot::channel();
		let spawn_handle = self.spawn_handle.clone();

		if let Ok(None) = inherent_data
			.get_data::<sp_core::ShufflingSeed>(&sp_ver::RANDOM_SEED_INHERENT_IDENTIFIER)
		{
			sp_ver::RandomSeedInherentDataProvider(Default::default())
				.provide_inherent_data(&mut inherent_data)
				.unwrap();
		}

		spawn_handle.spawn_blocking(
			"basic-authorship-proposer",
			Box::pin(async move {
				// leave some time for evaluation and block finalization (33%)
				let deadline = (self.now)() + max_duration - max_duration / 3;
				let res = self
					.propose_with(inherent_data, inherent_digests, deadline, block_size_limit)
					.await;
				if tx.send(res).is_err() {
					trace!("Could not send block production result to proposer!");
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
	C::Api: ApiExt<Block, StateBackend = backend::StateBackendFor<B, Block>>
		+ BlockBuilderApi<Block>
		+ EncryptedTxApi<Block>
		+ VerApi<Block>,
	PR: ProofRecording,
{

    fn get_decryption_key(&self, account_id: &AccountId32) -> sp_blockchain::Result<[u8;32]>{

		let keystore = self.keystore.clone();

        let api = self.client.runtime_api();
		let public_key = api.get_authority_public_key(&self.parent_id, account_id)?
            .ok_or(sp_encrypted_tx::Error::MissingPublicKey(account_id.clone()))?;

		debug!(target:"basic_authorship","public_key id:  {:?}", public_key);

		let key_pair = keystore.ecdsa_get_pair(KeyTypeId(*b"xxtx"), &public_key)
            .map_err(|_| sp_encrypted_tx::Error::CannotFindDecryptionKey(public_key.clone()))?
            .ok_or(sp_encrypted_tx::Error::CannotFindDecryptionKey(public_key))?;

		let seed: [u8; 32] = key_pair.seed();
		let priv_key: SecretKey = SecretKey::parse_slice(&seed).unwrap();

		let dummy_secret_key: SecretKey = SecretKey::default();
		let pub_key: PublicKey = PublicKey::from_secret_key(&dummy_secret_key);

		decapsulate(&pub_key, &priv_key)
            .map_err(|_| sp_blockchain::Error::Backend(String::from("cannot decapsulate aes key for decryption")))
    }

	async fn propose_with(
		self,
		inherent_data: InherentData,
		inherent_digests: DigestFor<Block>,
		deadline: time::Instant,
		block_size_limit: Option<usize>,
	) -> Result<Proposal<Block, backend::TransactionFor<B, Block>, PR::Proof>, sp_blockchain::Error>
	{
		let api = self.client.runtime_api();
        let next_block_number = self.parent_number.add(One::one()).add(One::one());
        let omit_transactions = api.is_new_session(&self.parent_id, next_block_number).unwrap();


		let mut block_builder =
			self.client.new_block_at(&self.parent_id, inherent_digests.clone(), PR::ENABLED)?;

        let block_builder_id = inherent_digests.logs().iter()
            .map(|l|
                <DigestItemFor<Block> as CompatibleDigestItem::<<sp_core::ed25519::Pair as Pair>::Signature>>::as_aura_pre_digest(&l)
            )
            .find(|d| d.is_some())
            .and_then(|id| id)
            .ok_or_else(|| sp_encrypted_tx::Error::UnknownBlockBuilder)?;


		let account_id = api.get_account_id(&self.parent_id, *block_builder_id)?
            .ok_or(sp_encrypted_tx::Error::UnknownCollatorId(*block_builder_id))?;
		debug!(target:"basic_authorship", "account id:  {:?}", account_id); 

		let doubly_encrypted_txs = api.get_double_encrypted_transactions(&self.parent_id, &account_id).unwrap();
		let singly_encrypted_txs = api.get_singly_encrypted_transactions(&self.parent_id, &account_id).unwrap();

		debug!(target:"basic_authorship", "found {} double encrypted transactions", doubly_encrypted_txs.len()); 
		debug!(target:"basic_authorship", "found {} singly encrypted transactions", singly_encrypted_txs.len()); 

		let decrypted_inherents = if !omit_transactions && (!singly_encrypted_txs.is_empty() || !doubly_encrypted_txs.is_empty()) {
			let aes_key = self.get_decryption_key(&account_id)?;
			debug!(target:"basic_authorship", "aes_key {:?}", aes_key); 
			let decrypted_inherents = singly_encrypted_txs.into_iter().map(|tx| {
				log::trace!(target:"basic_authorship", "decrypting singly encrypted call INPUT : {:?}", tx.data);
				let decrypted_msg = aes_decrypt(&aes_key, &tx.data).unwrap();
				log::trace!(target:"basic_authorship", "decrypting singly encrypted call OUTPUT: {:?}", decrypted_msg);
				api.create_submit_decrypted_transaction(&self.parent_id, tx.tx_id, decrypted_msg, 500000000).unwrap()
			});

			let singly_encrypted_inherents = doubly_encrypted_txs.into_iter().map(|tx| {
				log::trace!(target:"basic_authorship", "decrypting doubly encrypted call INPUT : {:?}", tx.data);
				let decrypted_msg = aes_decrypt(&aes_key, &tx.data).unwrap();
				log::trace!(target:"basic_authorship", "decrypting doubly encrypted call OUTPUT: {:?}", decrypted_msg);
				api.create_submit_singly_encrypted_transaction(&self.parent_id, tx.tx_id, decrypted_msg).unwrap()
			});
			decrypted_inherents.chain(singly_encrypted_inherents).collect()
		}else{
			vec![]
		};

		let (seed, inherents) = block_builder.create_inherents(inherent_data.clone())?;
		debug!(target:"block_builder", "found {} inherents", inherents.len());
		for inherent in inherents.into_iter().chain(decrypted_inherents.into_iter()) {
			debug!(target:"block_builder", "processing inherent");
            // TODO now it actually commits changes
			match block_builder.push(inherent) {
				Err(ApplyExtrinsicFailed(Validity(e))) if e.exhausted_resources() => {
					warn!("⚠️  Dropping non-mandatory inherent from overweight block.")
				},
				Err(ApplyExtrinsicFailed(Validity(e))) if e.was_mandatory() => {
					error!(
						"❌️ Mandatory inherent extrinsic returned error. Block cannot be produced."
					);
					Err(ApplyExtrinsicFailed(Validity(e)))?
				},
				Err(e) => {
					warn!("❗️ Inherent extrinsic returned unexpected error: {}. Dropping.", e);
				},
				Ok(_) => {
					trace!(target:"block_builder", "inherent EXECUTED & pushed into the block");
				},
			}
		}

		// proceed with transactions
		// We calculate soft deadline used only in case we start skipping transactions.
		let now = (self.now)();
		let soft_deadline = now + deadline.saturating_duration_since(now) / 2;
		let block_timer = time::Instant::now();
		let mut skipped = 0;
		let mut unqueue_invalid = Vec::new();
		block_builder.apply_previous_block_extrinsics(seed.clone());

		let mut t1 = self.transaction_pool.ready_at(self.parent_number).fuse();
		let mut t2 =
			futures_timer::Delay::new(deadline.saturating_duration_since((self.now)()) / 8).fuse();

		let mut pending_iterator = select! {
			res = t1 => res,
			_ = t2 => {
				log::warn!(
					"Timeout fired waiting for transaction pool at block #{}. \
					Proceeding with production.",
					self.parent_number,
				);
				self.transaction_pool.ready()
			},
		};

		let block_size_limit = block_size_limit.unwrap_or(self.default_block_size_limit);

		debug!("Attempting to push transactions from the pool.");
		debug!("Pool status: {:?}", self.transaction_pool.status());
		let mut transaction_pushed = false;
		let mut hit_block_size_limit = false;


		// after previous block is applied it is possible to prevalidate incomming transaction
		// but eventually changess needs to be rolled back, as those can be executed
		// only in the following(future) block
		api.execute_in_transaction(|api| {
			if omit_transactions {
				debug!(target:"block_builder", "new session starts in next block, omiting transaction from the pool");
				return TransactionOutcome::Rollback(());
			}
			while let Some(pending_tx) = pending_iterator.next() {
				let now = (self.now)();
				if now > deadline {
					debug!(target: "block_builder",
						"Consensus deadline reached when pushing block transactions, \
						proceeding with proposing."
					);
					break
				}

				let pending_tx_data = pending_tx.data().clone();
				let pending_tx_hash = pending_tx.hash().clone();

				let block_size =
					block_builder.estimate_block_size(self.include_proof_in_block_size_estimation);
				if block_size + pending_tx_data.encoded_size() > block_size_limit {
					pending_iterator.report_invalid(&pending_tx);
					if skipped < MAX_SKIPPED_TRANSACTIONS {
						skipped += 1;
						debug!(target: "block_builder",
							"Transaction would overflow the block size limit, \
							 but will try {} more transactions before quitting.",
							MAX_SKIPPED_TRANSACTIONS - skipped,
						);
						continue
					} else if now < soft_deadline {
						debug!(target: "block_builder",
							"Transaction would overflow the block size limit, \
							 but we still have time before the soft deadline, so \
							 we will try a bit more."
						);
						continue
					} else {
						debug!(target: "block_builder","Reached block size limit, proceeding with proposing.");
						hit_block_size_limit = true;
						break
					}
				}

				trace!("[{:?}] Pushing to the block.", pending_tx_hash);
				match sc_block_builder::BlockBuilder::push_with_api(
					&mut block_builder,
					api,
					pending_tx_data,
				) {
					Ok(()) => {
						transaction_pushed = true;
						debug!("[{:?}] Pushed to the block.", pending_tx_hash);
					},
					Err(ApplyExtrinsicFailed(Validity(e))) if e.exhausted_resources() => {
						pending_iterator.report_invalid(&pending_tx);
						if skipped < MAX_SKIPPED_TRANSACTIONS {
							skipped += 1;
							debug!(target: "block_builder",
								"Block seems full, but will try {} more transactions before quitting.",
								MAX_SKIPPED_TRANSACTIONS - skipped,
							);
						} else if (self.now)() < soft_deadline {
							debug!(target: "block_builder",
								"Block seems full, but we still have time before the soft deadline, \
								 so we will try a bit more before quitting."
							);
						} else {
							debug!(target: "block_builder","Block is full, proceed with proposing.");
							break
						}
					},
					Err(e) if skipped > 0 => {
						pending_iterator.report_invalid(&pending_tx);
						trace!(target: "block_builder",
							"[{:?}] Ignoring invalid transaction when skipping: {}",
							pending_tx_hash,
							e
						);
					},
					Err(e) => {
						pending_iterator.report_invalid(&pending_tx);
						debug!(target: "block_builder","[{:?}] Invalid transaction: {}", pending_tx_hash, e);
						unqueue_invalid.push(pending_tx_hash);
					},
				}
			}
			TransactionOutcome::Rollback(())
		});

		if hit_block_size_limit && !transaction_pushed {
			warn!(
				"Hit block size limit of `{}` without including any transaction!",
				block_size_limit,
			);
		}

		self.transaction_pool.remove_invalid(&unqueue_invalid);

		let (block, storage_changes, proof) = block_builder.build_with_seed(seed)?.into_inner();
        debug!(target: "block_builder","created block {:?}", block);

		self.metrics.report(|metrics| {
			metrics.number_of_transactions.set(block.extrinsics().len() as u64);
			metrics.block_constructed.observe(block_timer.elapsed().as_secs_f64());
		});

		info!(
			"🎁 Prepared block for proposing at {} [hash: {:?}; parent_hash: {}; extrinsics ({}): [{}]]",
			block.header().number(),
			<Block as BlockT>::Hash::from(block.header().hash()),
			block.header().parent_hash(),
			block.extrinsics().len(),
			block.extrinsics()
				.iter()
				.map(|xt| format!("{}", BlakeTwo256::hash_of(xt)))
				.collect::<Vec<_>>()
				.join(", ")
		);
		telemetry!(
			self.telemetry;
			CONSENSUS_INFO;
			"prepared_block_for_proposing";
			"number" => ?block.header().number(),
			"hash" => ?<Block as BlockT>::Hash::from(block.header().hash()),
		);

		if Decode::decode(&mut block.encode().as_slice()).as_ref() != Ok(&block) {
			error!("Failed to verify block encoding/decoding");
		}

		if let Err(err) =
			evaluation::evaluate_initial(&block, &self.parent_hash, self.parent_number)
		{
			error!("Failed to evaluate authored block: {:?}", err);
		}

		let proof =
			PR::into_proof(proof).map_err(|e| sp_blockchain::Error::Application(Box::new(e)))?;
		Ok(Proposal { block, proof, storage_changes })
	}
}
//
// #[cfg(test)]
// mod tests {
// 	use super::*;
//
// 	use futures::executor::block_on;
// 	use parking_lot::Mutex;
// 	use sc_client_api::Backend;
// 	use sc_transaction_pool::BasicPool;
// 	use sc_transaction_pool_api::{ChainEvent, MaintainedTransactionPool, TransactionSource};
// 	use sp_api::Core;
// 	use sp_blockchain::HeaderBackend;
// 	use sp_consensus::{BlockOrigin, Environment, Proposer};
// 	use sp_core::Pair;
// 	use sp_runtime::traits::NumberFor;
// 	use substrate_test_runtime_client::{
// 		prelude::*,
// 		runtime::{Block, Extrinsic, Transfer},
// 		TestClientBuilder, TestClientBuilderExt,
// 	};
//
// 	const SOURCE: TransactionSource = TransactionSource::External;
//
// 	fn extrinsic(nonce: u64) -> Extrinsic {
// 		Transfer {
// 			amount: Default::default(),
// 			nonce,
// 			from: AccountKeyring::Alice.into(),
// 			to: Default::default(),
// 		}
// 		.into_signed_tx()
// 	}
//
// 	fn exhausts_resources_extrinsic_from(who: usize) -> Extrinsic {
// 		let pair = AccountKeyring::numeric(who);
// 		let transfer = Transfer {
// 			// increase the amount to bump priority
// 			amount: 1,
// 			nonce: 0,
// 			from: pair.public(),
// 			to: Default::default(),
// 		};
// 		let signature = pair.sign(&transfer.encode()).into();
// 		Extrinsic::Transfer { transfer, signature, exhaust_resources_when_not_first: true }
// 	}
//
// 	fn chain_event<B: BlockT>(header: B::Header) -> ChainEvent<B>
// 	where
// 		NumberFor<B>: From<u64>,
// 	{
// 		ChainEvent::NewBestBlock { hash: header.hash(), tree_route: None }
// 	}
//
// 	#[test]
// 	fn should_cease_building_block_when_deadline_is_reached() {
// 		// given
// 		let client = Arc::new(substrate_test_runtime_client::new());
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
//
// 		block_on(txpool.submit_at(&BlockId::number(0), SOURCE, vec![extrinsic(0), extrinsic(1)]))
// 			.unwrap();
//
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(0u64))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
//
// 		let cell = Mutex::new((false, time::Instant::now()));
// 		let proposer = proposer_factory.init_with_now(
// 			&client.header(&BlockId::number(0)).unwrap().unwrap(),
// 			Box::new(move || {
// 				let mut value = cell.lock();
// 				if !value.0 {
// 					value.0 = true;
// 					return value.1
// 				}
// 				let old = value.1;
// 				let new = old + time::Duration::from_secs(1);
// 				*value = (true, new);
// 				old
// 			}),
// 		);
//
// 		// when
// 		let deadline = time::Duration::from_secs(3);
// 		let block =
// 			block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 				.map(|r| r.block)
// 				.unwrap();
//
// 		// then
// 		// block should have some extrinsics although we have some more in the pool.
// 		assert_eq!(block.extrinsics().len(), 1);
// 		assert_eq!(txpool.ready().count(), 2);
// 	}
//
// 	#[test]
// 	fn should_not_panic_when_deadline_is_reached() {
// 		let client = Arc::new(substrate_test_runtime_client::new());
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
//
// 		let cell = Mutex::new((false, time::Instant::now()));
// 		let proposer = proposer_factory.init_with_now(
// 			&client.header(&BlockId::number(0)).unwrap().unwrap(),
// 			Box::new(move || {
// 				let mut value = cell.lock();
// 				if !value.0 {
// 					value.0 = true;
// 					return value.1
// 				}
// 				let new = value.1 + time::Duration::from_secs(160);
// 				*value = (true, new);
// 				new
// 			}),
// 		);
//
// 		let deadline = time::Duration::from_secs(1);
// 		block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 			.map(|r| r.block)
// 			.unwrap();
// 	}
//
// 	#[test]
// 	fn proposed_storage_changes_should_match_execute_block_storage_changes() {
// 		env_logger::try_init();
// 		let (client, backend) = TestClientBuilder::new().build_with_backend();
// 		let client = Arc::new(client);
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
//
// 		let genesis_hash = client.info().best_hash;
// 		let block_id = BlockId::Hash(genesis_hash);
//
// 		block_on(txpool.submit_at(&BlockId::number(0), SOURCE, vec![extrinsic(0)])).unwrap();
//
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(0u64))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
//
// 		let proposer = proposer_factory.init_with_now(
// 			&client.header(&block_id).unwrap().unwrap(),
// 			Box::new(move || time::Instant::now()),
// 		);
//
// 		let deadline = time::Duration::from_secs(9);
// 		let proposal =
// 			block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 				.unwrap();
//
// 		assert_eq!(proposal.block.extrinsics().len(), 1);
//
// 		let api = client.runtime_api();
// 		let mut header = proposal.block.header.clone();
// 		let prev_header =
// 			backend.blockchain().header(BlockId::Hash(genesis_hash)).unwrap().unwrap();
// 		header.set_extrinsics_root(*prev_header.extrinsics_root());
// 		api.execute_block(&block_id, <Block as BlockT>::new(header, vec![])).unwrap();
//
// 		let state = backend.state_at(block_id).unwrap();
// 		let changes_trie_state =
// 			backend::changes_tries_state_at_block(&block_id, backend.changes_trie_storage())
// 				.unwrap();
//
// 		let storage_changes = api
// 			.into_storage_changes(&state, changes_trie_state.as_ref(), genesis_hash)
// 			.unwrap();
//
// 		assert_eq!(
// 			proposal.storage_changes.transaction_storage_root,
// 			storage_changes.transaction_storage_root,
// 		);
// 	}
//
// 	#[test]
// 	fn should_not_remove_invalid_transactions_when_skipping() {
// 		env_logger::try_init();
// 		// given
// 		let mut client = Arc::new(substrate_test_runtime_client::new());
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
//
// 		block_on(txpool.submit_at(
// 			&BlockId::number(0),
// 			SOURCE,
// 			vec![
// 				extrinsic(0),
// 				extrinsic(1),
// 				Transfer {
// 					amount: Default::default(),
// 					nonce: 2,
// 					from: AccountKeyring::Alice.into(),
// 					to: Default::default(),
// 				}.into_resources_exhausting_tx(),
// 				extrinsic(3),
// 				Transfer {
// 					amount: Default::default(),
// 					nonce: 4,
// 					from: AccountKeyring::Alice.into(),
// 					to: Default::default(),
// 				}.into_resources_exhausting_tx(),
// 				extrinsic(5),
// 				extrinsic(6),
// 			],
// 		))
// 		.unwrap();
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
// 		let mut propose_block = |client: &TestClient,
// 		                         number,
// 		                         expected_block_extrinsics,
// 		                         expected_pool_transactions| {
// 			let proposer = proposer_factory.init_with_now(
// 				&client.header(&BlockId::number(number)).unwrap().unwrap(),
// 				Box::new(move || time::Instant::now()),
// 			);
//
// 			// when
// 			let deadline = time::Duration::from_secs(900);
// 			let block =
// 				block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 					.map(|r| r.block)
// 					.unwrap();
//
// 			// then
// 			// block should have some extrinsics although we have some more in the pool.
// 			assert_eq!(block.extrinsics().len(), expected_block_extrinsics);
// 			assert_eq!(txpool.ready().count(), expected_pool_transactions);
//
// 			block
// 		};
//
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(0u64))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
// 		assert_eq!(txpool.ready().count(), 7);
//
// 		// let's create one block and import it
// 		let block = propose_block(&client, 0, 2, 7);
// 		let block_hash = block.header().hash();
// 		block_on(client.import(BlockOrigin::Own, block)).unwrap();
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(1))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
// 		assert_eq!(txpool.ready().count(), 5);
//
// 		// push one extra block - extrinsics in the pool makred as 'exhausted_resources'
// 		// to succeed needs to be executed as first in the processed block. Due to
// 		// modifications in block_builder all extrinsics from previous block are applied
// 		// beofre trying to validate extrinsics from the tx pool. Once we include empty
// 		// block in between 'exhausted_resources' extrinsic from the pool is exeucted as
// 		// the first one and the origin test logic is maintained
// 		let block = client
// 			.new_block_at(&BlockId::Hash(block_hash), Default::default(), false)
// 			.unwrap()
// 			.build_with_seed(Default::default())
// 			.unwrap();
// 		block_on(client.import(BlockOrigin::Own, block.block)).unwrap();
//
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(2))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
// 		assert_eq!(txpool.ready().count(), 5);
//
// 		// now let's make sure that we can still make some progress
// 		let block = propose_block(&client, 2, 2, 5);
// 		// block_on(client.import(BlockOrigin::Own, block)).unwrap();
// 	}
//
// 	#[test]
// 	fn should_cease_building_block_when_block_limit_is_reached() {
// 		let client = Arc::new(substrate_test_runtime_client::new());
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
// 		let genesis_header = client
// 			.header(&BlockId::Number(0u64))
// 			.expect("header get error")
// 			.expect("there should be header");
//
// 		let extrinsics_num = 4;
// 		let extrinsics = (0..extrinsics_num)
// 			.map(|v| Extrinsic::IncludeData(vec![v as u8; 10]))
// 			.collect::<Vec<_>>();
//
// 		let block_limit = genesis_header.encoded_size() +
// 			extrinsics
// 				.iter()
// 				.take(extrinsics_num - 1)
// 				.map(Encode::encoded_size)
// 				.sum::<usize>() +
// 			Vec::<Extrinsic>::new().encoded_size();
//
// 		block_on(txpool.submit_at(&BlockId::number(0), SOURCE, extrinsics)).unwrap();
//
// 		block_on(txpool.maintain(chain_event(genesis_header.clone())));
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
//
// 		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();
//
// 		// Give it enough time
// 		let deadline = time::Duration::from_secs(300);
// 		let block = block_on(proposer.propose(
// 			Default::default(),
// 			Default::default(),
// 			deadline,
// 			Some(block_limit),
// 		))
// 		.map(|r| r.block)
// 		.unwrap();
//
// 		// Based on the block limit, one transaction shouldn't be included.
// 		assert_eq!(block.extrinsics().len(), extrinsics_num - 1);
//
// 		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();
//
// 		let block =
// 			block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 				.map(|r| r.block)
// 				.unwrap();
//
// 		// Without a block limit we should include all of them
// 		assert_eq!(block.extrinsics().len(), extrinsics_num);
//
// 		let mut proposer_factory = ProposerFactory::with_proof_recording(
// 			spawner.clone(),
// 			client.clone(),
// 			txpool.clone(),
// 			None,
// 			None,
// 		);
//
// 		let proposer = block_on(proposer_factory.init(&genesis_header)).unwrap();
//
// 		// Give it enough time
// 		let block = block_on(proposer.propose(
// 			Default::default(),
// 			Default::default(),
// 			deadline,
// 			Some(block_limit),
// 		))
// 		.map(|r| r.block)
// 		.unwrap();
//
// 		// The block limit didn't changed, but we now include the proof in the estimation of the
// 		// block size and thus, one less transaction should fit into the limit.
// 		assert_eq!(block.extrinsics().len(), extrinsics_num - 2);
// 	}
//
// 	#[test]
// 	fn should_keep_adding_transactions_after_exhausts_resources_before_soft_deadline() {
// 		// given
// 		let client = Arc::new(substrate_test_runtime_client::new());
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
//
// 		block_on(
// 			txpool.submit_at(
// 				&BlockId::number(0),
// 				SOURCE,
// 				// add 2 * MAX_SKIPPED_TRANSACTIONS that exhaust resources
// 				(0..MAX_SKIPPED_TRANSACTIONS * 2)
// 					.into_iter()
// 					.map(|i| exhausts_resources_extrinsic_from(i))
// 					// and some transactions that are okay.
// 					.chain((0..MAX_SKIPPED_TRANSACTIONS).into_iter().map(|i| extrinsic(i as _)))
// 					.collect(),
// 			),
// 		)
// 		.unwrap();
//
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(0u64))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
// 		assert_eq!(txpool.ready().count(), MAX_SKIPPED_TRANSACTIONS * 3);
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
//
// 		let cell = Mutex::new(time::Instant::now());
// 		let proposer = proposer_factory.init_with_now(
// 			&client.header(&BlockId::number(0)).unwrap().unwrap(),
// 			Box::new(move || {
// 				let mut value = cell.lock();
// 				let old = *value;
// 				*value = old + time::Duration::from_secs(1);
// 				old
// 			}),
// 		);
//
// 		// when
// 		// give it enough time so that deadline is never triggered.
// 		let deadline = time::Duration::from_secs(900);
// 		let block =
// 			block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 				.map(|r| r.block)
// 				.unwrap();
//
// 		// then block should have all non-exhaust resources extrinsics (+ the first one).
// 		assert_eq!(block.extrinsics().len(), MAX_SKIPPED_TRANSACTIONS + 1);
// 	}
//
// 	#[test]
// 	fn should_only_skip_up_to_some_limit_after_soft_deadline() {
// 		// given
// 		let client = Arc::new(substrate_test_runtime_client::new());
// 		let spawner = sp_core::testing::TaskExecutor::new();
// 		let txpool = BasicPool::new_full(
// 			Default::default(),
// 			true.into(),
// 			None,
// 			spawner.clone(),
// 			client.clone(),
// 		);
//
// 		block_on(
// 			txpool.submit_at(
// 				&BlockId::number(0),
// 				SOURCE,
// 				(0..MAX_SKIPPED_TRANSACTIONS + 2)
// 					.into_iter()
// 					.map(|i| exhausts_resources_extrinsic_from(i))
// 					// and some transactions that are okay.
// 					.chain((0..MAX_SKIPPED_TRANSACTIONS).into_iter().map(|i| extrinsic(i as _)))
// 					.collect(),
// 			),
// 		)
// 		.unwrap();
//
// 		block_on(
// 			txpool.maintain(chain_event(
// 				client
// 					.header(&BlockId::Number(0u64))
// 					.expect("header get error")
// 					.expect("there should be header"),
// 			)),
// 		);
// 		assert_eq!(txpool.ready().count(), MAX_SKIPPED_TRANSACTIONS * 2 + 2);
//
// 		let mut proposer_factory =
// 			ProposerFactory::new(spawner.clone(), client.clone(), txpool.clone(), None, None);
//
// 		let deadline = time::Duration::from_secs(600);
// 		let cell = Arc::new(Mutex::new((0, time::Instant::now())));
// 		let cell2 = cell.clone();
// 		let proposer = proposer_factory.init_with_now(
// 			&client.header(&BlockId::number(0)).unwrap().unwrap(),
// 			Box::new(move || {
// 				let mut value = cell.lock();
// 				let (called, old) = *value;
// 				// add time after deadline is calculated internally (hence 1)
// 				let increase = if called == 1 {
// 					// we start after the soft_deadline should have already been reached.
// 					deadline / 2
// 				} else {
// 					// but we make sure to never reach the actual deadline
// 					time::Duration::from_millis(0)
// 				};
// 				*value = (called + 1, old + increase);
// 				old
// 			}),
// 		);
//
// 		let block =
// 			block_on(proposer.propose(Default::default(), Default::default(), deadline, None))
// 				.map(|r| r.block)
// 				.unwrap();
//
// 		// then the block should have no transactions despite some in the pool
// 		assert_eq!(block.extrinsics().len(), 1);
// 		assert!(
// 			cell2.lock().0 > MAX_SKIPPED_TRANSACTIONS,
// 			"Not enough calls to current time, which indicates the test might have ended because of deadline, not soft deadline"
// 		);
// 	}
// }
