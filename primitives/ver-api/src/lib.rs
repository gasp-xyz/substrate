#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Codec, Decode, Encode};
use sp_runtime::{traits::Block as BlockT, AccountId32};
use sp_std::vec::Vec;

/// Information about extrinsic fetched from runtime API
#[derive(Encode, Decode, PartialEq)]
pub struct ExtrinsicInfo {
	/// extrinsic signer
	pub who: AccountId32,
}

sp_api::decl_runtime_apis! {
	/// The `VerApi` api trait for fetching information about extrinsic author and
	/// nonce
	pub trait VerApi {
		// TODO: make AccountId generic
		/// Provides information about extrinsic signer and nonce
		fn get_signer(tx: <Block as BlockT>::Extrinsic) -> Option<(AccountId32, u32)>;

		/// Checks if storage migration is scheuled
		fn is_storage_migration_scheduled() -> bool;

		/// stores shuffling seed for current block & shuffles
		/// previous block extrinsics if any enqueued
		fn store_seed(seed: sp_core::H256);

		// pops single tx from storage queue
		fn pop_tx() -> Option<Vec<u8>>;

		// creates inherent that injects new txs into storage queue
		fn can_enqueue_txs() -> bool;

		// creates inherent that injects new txs into storage queue
		fn create_enqueue_txs_inherent(txs: Vec<Block::Extrinsic>) -> Block::Extrinsic;

	}

	pub trait VerNonceApi<Account> where
	Account : Codec
	{
		/// fetch number of enqueued txs from given account
		fn enqueued_txs_count(account: Account) -> u64;
	}
}
