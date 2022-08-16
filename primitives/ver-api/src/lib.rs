#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
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

		/// Checks if given block will start new session
		fn store_seed(seed: sp_core::H256);

		/// Checks if given block will start new session
		fn store_txs(seed: Vec<Vec<u8>>);

		fn pop_txs() -> Vec<Vec<u8>>;

		fn create_enqueue_txs_inherent(txs: Vec<Vec<u8>>) -> Block::Extrinsic;
	}
}
