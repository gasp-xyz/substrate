// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
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

use frame_support::{
	parameter_types,
	traits::{
		ConstU32, ConstU64, Currency, GenesisBuild, LockableCurrency, SignedImbalance,
		WithdrawReasons,
	},
};
use sp_core::H256;
use sp_runtime::{
	testing::Header,
	traits::{BlakeTwo256, Identity, IdentityLookup},
};

use self::imbalances::{NegativeImbalance, PositiveImbalance};

use super::*;
use crate as pallet_vesting_mangata;

pub const TKN: u32 = 0;

pub(crate) type Balance = u64;
pub(crate) type AccountId = u64;
pub(crate) type TokenId = u32;
pub(crate) type BlockNumber = u64;

type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Test>;
type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test where
		Block = Block,
		NodeBlock = Block,
		UncheckedExtrinsic = UncheckedExtrinsic,
	{
		System: frame_system::{Pallet, Call, Config, Storage, Event<T>},
		Balances: pallet_balances::{Pallet, Call, Storage, Config<T>, Event<T>},
		Vesting: pallet_vesting_mangata::{Pallet, Call, Storage, Event<T>, Config<T>},
	}
);

impl frame_system::Config for Test {
	type AccountData = pallet_balances::AccountData<Balance>;
	type AccountId = AccountId;
	type BaseCallFilter = frame_support::traits::Everything;
	type BlockHashCount = ConstU64<250>;
	type BlockLength = ();
	type BlockNumber = BlockNumber;
	type BlockWeights = ();
	type RuntimeCall = RuntimeCall;
	type DbWeight = ();
	type RuntimeEvent = RuntimeEvent;
	type Hash = H256;
	type Hashing = BlakeTwo256;
	type Header = Header;
	type Index = u64;
	type Lookup = IdentityLookup<Self::AccountId>;
	type OnKilledAccount = ();
	type OnNewAccount = ();
	type OnSetCode = ();
	type MaxConsumers = frame_support::traits::ConstU32<16>;
	type RuntimeOrigin = RuntimeOrigin;
	type PalletInfo = PalletInfo;
	type SS58Prefix = ();
	type SystemWeightInfo = ();
	type Version = ();
}

impl pallet_balances::Config for Test {
	type AccountStore = System;
	type Balance = Balance;
	type DustRemoval = ();
	type RuntimeEvent = RuntimeEvent;
	type ExistentialDeposit = ExistentialDeposit;
	type MaxLocks = ConstU32<10>;
	type MaxReserves = ();
	type ReserveIdentifier = [u8; 8];
	type WeightInfo = ();
	type FreezeIdentifier = ();
	type MaxFreezes = ();
	type HoldIdentifier = ();
	type MaxHolds = ();
}
parameter_types! {
	pub const MinVestedTransfer: Balance = 256 * 2;
	pub UnvestedFundsAllowedWithdrawReasons: WithdrawReasons =
		WithdrawReasons::except(WithdrawReasons::TRANSFER | WithdrawReasons::RESERVE);
	pub static ExistentialDeposit: Balance = 1;
}
impl Config for Test {
	type BlockNumberToBalance = Identity;
	type Tokens = MultiTokenCurrencyAdapter;
	type RuntimeEvent = RuntimeEvent;
	const MAX_VESTING_SCHEDULES: u32 = 3;
	type MinVestedTransfer = MinVestedTransfer;
	type WeightInfo = ();
	type UnvestedFundsAllowedWithdrawReasons = UnvestedFundsAllowedWithdrawReasons;
}

pub struct ExtBuilder {
	existential_deposit: Balance,
	vesting_genesis_config: Option<Vec<(AccountId, TokenId, u64, u64, Balance)>>,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self { existential_deposit: 1, vesting_genesis_config: None }
	}
}

impl ExtBuilder {
	pub fn existential_deposit(mut self, existential_deposit: Balance) -> Self {
		self.existential_deposit = existential_deposit;
		self
	}

	pub fn vesting_genesis_config(
		mut self,
		config: Vec<(AccountId, TokenId, u64, u64, Balance)>,
	) -> Self {
		self.vesting_genesis_config = Some(config);
		self
	}

	pub fn build(self) -> sp_io::TestExternalities {
		EXISTENTIAL_DEPOSIT.with(|v| *v.borrow_mut() = self.existential_deposit);
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		pallet_balances::GenesisConfig::<Test> {
			balances: vec![
				(1, 10 * self.existential_deposit),
				(2, 20 * self.existential_deposit),
				(3, 30 * self.existential_deposit),
				(4, 40 * self.existential_deposit),
				(12, 10 * self.existential_deposit),
				(13, 9999 * self.existential_deposit),
			],
		}
		.assimilate_storage(&mut t)
		.unwrap();

		let vesting = if let Some(vesting_config) = self.vesting_genesis_config {
			vesting_config
		} else {
			vec![
				// locked = free - liquid
				(1, TKN, 0, 10, (10 - 5) * self.existential_deposit),
				(2, TKN, 10, 20, 20 * self.existential_deposit),
				(12, TKN, 10, 20, (10 - 5) * self.existential_deposit),
			]
		};

		pallet_vesting_mangata::GenesisConfig::<Test> { vesting }
			.assimilate_storage(&mut t)
			.unwrap();
		let mut ext = sp_io::TestExternalities::new(t);
		ext.execute_with(|| System::set_block_number(1));
		ext
	}
}

pub struct MultiTokenCurrencyAdapter;
impl MultiTokenCurrency<AccountId> for MultiTokenCurrencyAdapter {
	type Balance = Balance;
	type CurrencyId = TokenId;
	type PositiveImbalance = PositiveImbalance<Test>;
	type NegativeImbalance = NegativeImbalance<Test>;

	fn total_balance(_currency_id: Self::CurrencyId, who: &AccountId) -> Self::Balance {
		Balances::total_balance(who)
	}

	fn can_slash(_currency_id: Self::CurrencyId, who: &AccountId, value: Self::Balance) -> bool {
		Balances::can_slash(who, value)
	}

	fn total_issuance(_currency_id: Self::CurrencyId) -> Self::Balance {
		Balances::total_issuance()
	}

	fn minimum_balance(_currency_id: Self::CurrencyId) -> Self::Balance {
		Balances::minimum_balance()
	}

	fn burn(_currency_id: Self::CurrencyId, amount: Self::Balance) -> Self::PositiveImbalance {
		Balances::burn(amount).into()
	}

	fn issue(_currency_id: Self::CurrencyId, amount: Self::Balance) -> Self::NegativeImbalance {
		Balances::issue(amount).into()
	}

	fn free_balance(_currency_id: Self::CurrencyId, who: &AccountId) -> Self::Balance {
		Balances::free_balance(who)
	}

	fn ensure_can_withdraw(
		currency_id: Self::CurrencyId,
		who: &AccountId,
		amount: Self::Balance,
		reasons: frame_support::traits::WithdrawReasons,
		_new_balance: Self::Balance,
	) -> frame_support::pallet_prelude::DispatchResult {
		let new_balance = Self::free_balance(currency_id, who)
			.checked_sub(amount)
			.ok_or(pallet_balances::Error::<Test>::InsufficientBalance)?;
		Balances::ensure_can_withdraw(who, amount, reasons, new_balance)
	}

	fn transfer(
		_currency_id: Self::CurrencyId,
		source: &AccountId,
		dest: &AccountId,
		value: Self::Balance,
		existence_requirement: frame_support::traits::ExistenceRequirement,
	) -> frame_support::pallet_prelude::DispatchResult {
		<Balances as frame_support::traits::Currency<AccountId>>::transfer(
			source,
			dest,
			value,
			existence_requirement,
		)
	}

	fn slash(
		_currency_id: Self::CurrencyId,
		who: &AccountId,
		value: Self::Balance,
	) -> (Self::NegativeImbalance, Self::Balance) {
		let (imbalance, balance) = Balances::slash(who, value);
		(imbalance.into(), balance)
	}

	fn deposit_into_existing(
		_currency_id: Self::CurrencyId,
		who: &AccountId,
		value: Self::Balance,
	) -> core::result::Result<Self::PositiveImbalance, sp_runtime::DispatchError> {
		Balances::deposit_into_existing(who, value).map(|imbalance| imbalance.into())
	}

	fn deposit_creating(
		_currency_id: Self::CurrencyId,
		who: &AccountId,
		value: Self::Balance,
	) -> Self::PositiveImbalance {
		Balances::deposit_creating(who, value).into()
	}

	fn withdraw(
		_currency_id: Self::CurrencyId,
		who: &AccountId,
		value: Self::Balance,
		reasons: frame_support::traits::WithdrawReasons,
		liveness: frame_support::traits::ExistenceRequirement,
	) -> core::result::Result<Self::NegativeImbalance, sp_runtime::DispatchError> {
		Balances::withdraw(who, value, reasons, liveness).map(|imbalance| imbalance.into())
	}

	fn make_free_balance_be(
		_currency_id: Self::CurrencyId,
		who: &AccountId,
		balance: Self::Balance,
	) -> frame_support::traits::SignedImbalance<Self::Balance, Self::PositiveImbalance> {
		match Balances::make_free_balance_be(who, balance) {
			SignedImbalance::Positive(imbalance) => SignedImbalance::Positive(imbalance.into()),
			SignedImbalance::Negative(imbalance) => SignedImbalance::Negative(imbalance.into()),
		}
	}
}

impl MultiTokenLockableCurrency<AccountId> for MultiTokenCurrencyAdapter {
	type Moment = BlockNumber;

	type MaxLocks = ();

	fn set_lock(
		_currency_id: Self::CurrencyId,
		id: frame_support::traits::LockIdentifier,
		who: &AccountId,
		amount: Self::Balance,
		reasons: frame_support::traits::WithdrawReasons,
	) {
		Balances::set_lock(id, who, amount, reasons)
	}

	fn extend_lock(
		_currency_id: Self::CurrencyId,
		id: frame_support::traits::LockIdentifier,
		who: &AccountId,
		amount: Self::Balance,
		reasons: frame_support::traits::WithdrawReasons,
	) {
		Balances::extend_lock(id, who, amount, reasons)
	}

	fn remove_lock(
		_currency_id: Self::CurrencyId,
		id: frame_support::traits::LockIdentifier,
		who: &AccountId,
	) {
		Balances::remove_lock(id, who)
	}
}

mod imbalances {
	// wrapping these imbalances in a private module is necessary to ensure absolute
	// privacy of the inner member.
	use frame_support::traits::{
		tokens::currency::MultiTokenImbalanceWithZeroTrait, Imbalance, SameOrOther, TryDrop,
	};
	use pallet_balances::{Config, TotalIssuance};
	use sp_runtime::traits::{Saturating, Zero};
	use sp_std::{mem, result};

	use super::{TokenId, TKN};

	impl<T: Config> MultiTokenImbalanceWithZeroTrait<TokenId> for PositiveImbalance<T> {
		fn from_zero(currency_id: TokenId) -> Self {
			Self::zero(currency_id)
		}
	}

	impl<T: Config> MultiTokenImbalanceWithZeroTrait<TokenId> for NegativeImbalance<T> {
		fn from_zero(currency_id: TokenId) -> Self {
			Self::zero(currency_id)
		}
	}

	/// Opaque, move-only struct with private fields that serves as a token
	/// denoting that funds have been created without any equal and opposite
	/// accounting.
	#[must_use]
	pub struct PositiveImbalance<T: Config>(TokenId, T::Balance);

	impl<T: Config> PositiveImbalance<T> {
		/// Create a new positive imbalance from a balance.
		pub fn new(currency_id: TokenId, amount: T::Balance) -> Self {
			PositiveImbalance(currency_id, amount)
		}

		pub fn zero(currency_id: TokenId) -> Self {
			PositiveImbalance(currency_id, Zero::zero())
		}
	}

	impl<T: Config> Default for PositiveImbalance<T> {
		fn default() -> Self {
			PositiveImbalance(Default::default(), Default::default())
		}
	}

	/// Opaque, move-only struct with private fields that serves as a token
	/// denoting that funds have been destroyed without any equal and opposite
	/// accounting.
	#[must_use]
	pub struct NegativeImbalance<T: Config>(pub TokenId, T::Balance);

	impl<T: Config> NegativeImbalance<T> {
		/// Create a new negative imbalance from a balance.
		pub fn new(currency_id: TokenId, amount: T::Balance) -> Self {
			NegativeImbalance(currency_id, amount)
		}

		pub fn zero(currency_id: TokenId) -> Self {
			NegativeImbalance(currency_id, Zero::zero())
		}
	}

	impl<T: Config> Default for NegativeImbalance<T> {
		fn default() -> Self {
			NegativeImbalance(Default::default(), Default::default())
		}
	}

	impl<T: Config> TryDrop for PositiveImbalance<T> {
		fn try_drop(self) -> result::Result<(), Self> {
			self.drop_zero()
		}
	}

	impl<T: Config> Imbalance<T::Balance> for PositiveImbalance<T> {
		type Opposite = NegativeImbalance<T>;

		fn zero() -> Self {
			unimplemented!("PositiveImbalance::zero is not implemented");
		}

		fn drop_zero(self) -> result::Result<(), Self> {
			if self.1.is_zero() {
				Ok(())
			} else {
				Err(self)
			}
		}
		fn split(self, amount: T::Balance) -> (Self, Self) {
			let first = self.1.min(amount);
			let second = self.1 - first;
			let currency_id = self.0;

			mem::forget(self);
			(Self::new(currency_id, first), Self::new(currency_id, second))
		}
		fn merge(mut self, other: Self) -> Self {
			assert_eq!(self.0, other.0);
			self.1 = self.1.saturating_add(other.1);
			mem::forget(other);
			self
		}
		fn subsume(&mut self, other: Self) {
			assert_eq!(self.0, other.0);
			self.1 = self.1.saturating_add(other.1);
			mem::forget(other);
		}
		// allow to make the impl same with `pallet-balances`
		#[allow(clippy::comparison_chain)]
		fn offset(self, other: Self::Opposite) -> SameOrOther<Self, Self::Opposite> {
			assert_eq!(self.0, other.0);
			let (a, b) = (self.1, other.1);
			let currency_id = self.0;
			mem::forget((self, other));

			if a > b {
				SameOrOther::Same(Self::new(currency_id, a - b))
			} else if b > a {
				SameOrOther::Other(NegativeImbalance::new(currency_id, b - a))
			} else {
				SameOrOther::None
			}
		}
		fn peek(&self) -> T::Balance {
			self.1
		}
	}

	impl<T: Config> TryDrop for NegativeImbalance<T> {
		fn try_drop(self) -> result::Result<(), Self> {
			self.drop_zero()
		}
	}

	impl<T: Config> Imbalance<T::Balance> for NegativeImbalance<T> {
		type Opposite = PositiveImbalance<T>;

		fn zero() -> Self {
			unimplemented!("NegativeImbalance::zero is not implemented");
		}
		fn drop_zero(self) -> result::Result<(), Self> {
			if self.1.is_zero() {
				Ok(())
			} else {
				Err(self)
			}
		}
		fn split(self, amount: T::Balance) -> (Self, Self) {
			let first = self.1.min(amount);
			let second = self.1 - first;
			let currency_id = self.0;

			mem::forget(self);
			(Self::new(currency_id, first), Self::new(currency_id, second))
		}
		fn merge(mut self, other: Self) -> Self {
			assert_eq!(self.0, other.0);
			self.1 = self.1.saturating_add(other.1);
			mem::forget(other);
			self
		}
		fn subsume(&mut self, other: Self) {
			assert_eq!(self.0, other.0);
			self.1 = self.1.saturating_add(other.1);
			mem::forget(other);
		}
		// allow to make the impl same with `pallet-balances`
		#[allow(clippy::comparison_chain)]
		fn offset(self, other: Self::Opposite) -> SameOrOther<Self, Self::Opposite> {
			assert_eq!(self.0, other.0);
			let (a, b) = (self.1, other.1);
			let currency_id = self.0;
			mem::forget((self, other));
			if a > b {
				SameOrOther::Same(Self::new(currency_id, a - b))
			} else if b > a {
				SameOrOther::Other(PositiveImbalance::new(currency_id, b - a))
			} else {
				SameOrOther::None
			}
		}
		fn peek(&self) -> T::Balance {
			self.1
		}
	}

	impl<T: Config> Drop for PositiveImbalance<T> {
		/// Basic drop handler will just square up the total issuance.
		fn drop(&mut self) {
			<TotalIssuance<T>>::mutate(|v| *v = v.saturating_add(self.1));
		}
	}

	impl<T: Config> Drop for NegativeImbalance<T> {
		/// Basic drop handler will just square up the total issuance.
		fn drop(&mut self) {
			<TotalIssuance<T>>::mutate(|v| *v = v.saturating_sub(self.1));
		}
	}

	impl<T: Config> From<pallet_balances::PositiveImbalance<T>> for PositiveImbalance<T> {
		fn from(value: pallet_balances::PositiveImbalance<T>) -> Self {
			PositiveImbalance::new(TKN, value.peek())
		}
	}

	impl<T: Config> From<pallet_balances::NegativeImbalance<T>> for NegativeImbalance<T> {
		fn from(value: pallet_balances::NegativeImbalance<T>) -> Self {
			NegativeImbalance::new(TKN, value.peek())
		}
	}
}
