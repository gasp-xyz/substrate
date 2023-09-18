use crate::hash::{H256, H512};
use codec::{Decode, Encode};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use scale_info::TypeInfo;

#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq, Default, TypeInfo)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
/// stores information needed to verify if
/// shuffling seed was generated properly
pub struct ShufflingSeed {
	/// shuffling seed for the previous block
	pub seed: H256,
	/// seed signature
	pub proof: H512,
}
