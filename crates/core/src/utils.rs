use alloy::primitives::Address;
use stf::Hash;

pub fn address_to_hash(addr: &Address) -> Hash {
    let mut hash = Hash::default();
    hash[12..].copy_from_slice(addr.as_slice());
    hash
}
