#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Decode, Encode};
use sp_runtime::traits::Block as BlockT;
use sp_runtime::AccountId32;
use sp_std::vec::Vec;


use sp_core::RuntimeDebug;
use frame_support::weights::Weight;

#[derive(Clone, PartialEq, Eq, Encode, Decode, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(serde::Serialize, serde::Deserialize))]
pub enum ExtrinsicType<Hash>{
	DoublyEncryptedTx{
        doubly_encrypted_call: Vec<u8>,
        nonce: u32,
        weight: Weight,
        builder: AccountId32,
        executor: AccountId32,
	},
    SinglyEncryptedTx{
        identifier: Hash,
        singly_encrypted_call: Vec<u8>,
    },
    DecryptedTx{
        identifier: Hash,
        decrypted_call: Vec<u8>,
    },
	Other,
}

#[derive(Debug, thiserror::Error)]
pub enum Error{
	#[error("Missing public key for account {0}")]
	MissingPublicKey(sp_runtime::AccountId32),

	#[error("Cannot find account id of collator: {0}")]
	UnknownCollatorId(u64),

	#[error("Block builder is unknown")]
	UnknownBlockBuilder,

	#[error("Cannot find decryption key for public key {0}")]
	CannotFindDecryptionKey(sp_core::ecdsa::Public),

	#[error("{0} didnt decrypt doubly encrypted transaction")]
	MissingSinglyEncryptedTransaction(sp_runtime::AccountId32),

	#[error("{0} didnt decrypt singly encrypted transaction")]
	MissingDecryptedTransaction(sp_runtime::AccountId32),

	#[error("Unexpected decrypting transaction")]
	UnexpectedDecryptionTransaction,

	#[error("Decrypted payload mismatch")]
    DecryptedPayloadMismatch,
}


#[derive(Encode, Decode, PartialEq, Debug)]
pub struct EncryptedTx<Hash>{
    pub tx_id: Hash,
    pub data: Vec<u8>,
}

sp_api::decl_runtime_apis! {
	pub trait EncryptedTxApi
    {
        // creates extrinsic that decrypts doubly encrypted transaction
		fn create_submit_singly_encrypted_transaction(identifier: <Block as BlockT>::Hash, singly_encrypted_call: Vec<u8>) -> <Block as BlockT>::Extrinsic;

        // creates extrinsic that decrypts singly encrypted transaction
        fn create_submit_decrypted_transaction(identifier: <Block as BlockT>::Hash, decrypted_call: Vec<u8>, weight: Weight) -> <Block as BlockT>::Extrinsic;

		/// parses information about extrinsic
		fn get_type(extrinsic: <Block as BlockT>::Extrinsic) -> ExtrinsicType<<Block as BlockT>::Hash>;

        // fetches double encrypted transactions from FIFO queue
		fn get_double_encrypted_transactions(block_builder_id: &AccountId32) -> Vec<EncryptedTx<<Block as BlockT>::Hash>>;

        // fetches singly encrypted transactions from FIFO queue
		fn get_singly_encrypted_transactions(block_builder_id: &AccountId32) -> Vec<EncryptedTx<<Block as BlockT>::Hash>>;

        // fetches address assigned to authority id
		fn get_account_id(block_builder_id: u64) -> Option<AccountId32>;

        // use autority id to identify public key (from encrypted transactions apllet)
		fn get_authority_public_key(authority_id: &AccountId32) -> Option<sp_core::ecdsa::Public>;
	}
}
