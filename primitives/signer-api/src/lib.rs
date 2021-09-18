#![cfg_attr(not(feature = "std"), no_std)]
use sp_runtime::traits::Block as BlockT;
use sp_runtime::AccountId32;

sp_api::decl_runtime_apis! {
	/// The `SignerApi` api trait for fetching information about extrinsic author
	pub trait SignerApi {
		/// Provides information about extrinsic signer
		fn get_signer(tx: <Block as BlockT>::Extrinsic) -> Option<AccountId32>;
	}
}
