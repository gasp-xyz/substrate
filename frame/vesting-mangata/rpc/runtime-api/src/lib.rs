// This file is part of Substrate.

// Copyright (C) 2019-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Runtime API definition for transaction payment pallet.

#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Codec, Decode, Encode};

#[cfg(not(feature = "std"))]
use sp_std::{vec, vec::Vec};

use sp_runtime::traits::{MaybeDisplay, MaybeFromStr};

pub use pallet_vesting_mangata::{VestingInfo};

sp_api::decl_runtime_apis! {
	pub trait VestingMangataApi<AccountId, TokenId, Balance, BlockNumber> where
		AccountId: Codec + MaybeDisplay + MaybeFromStr,
		Balance: Codec + MaybeDisplay + MaybeFromStr,
		TokenId: Codec + MaybeDisplay + MaybeFromStr,
		BlockNumber: Codec + MaybeDisplay + MaybeFromStr,
	{
		fn get_vesting_locked_at(who: AccountId, token_id: TokenId, at_block_number: Option<BlockNumber>) -> Vec<(VestingInfo<Balance, BlockNumber>, Balance)>;
	}
}

