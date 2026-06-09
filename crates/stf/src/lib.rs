#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolValue, sol};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256, Sha3_256};

#[derive(Debug, Clone)]
pub enum StfError {
    InsufficientBalance,
    BalanceOverflow,
    MerkleProofVerificationFailed,
    InvalidNonce { expected: u64, got: u64 },
    InvalidTransaction(String),
}

pub enum TransactionResult {
    Success(Account, Account),
    Failure(Account), // `from` account after changes (nonce, fee)
}

pub type Hash = [u8; 32];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Transaction {
    pub from: Hash,
    pub to: Hash,
    pub nonce: u64,
    pub fee: u64,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Account {
    pub id: Hash,
    pub balance: u64,
    pub nonce: u64,
}

impl Account {
    pub fn new(id: Hash) -> Self {
        Self {
            id,
            nonce: 0,
            balance: 0,
        }
    }

    pub fn hash(&self) -> Hash {
        let mut data = Vec::new();
        data.extend_from_slice(&self.nonce.to_be_bytes());
        data.extend_from_slice(&self.id);
        data.extend_from_slice(&self.balance.to_be_bytes());
        let mut hasher = Sha3_256::new();
        hasher.update(&data);
        hasher.finalize().into()
    }
}

fn hash_nodes(left: &Hash, right: &Hash) -> Hash {
    let mut data = Vec::new();
    data.extend_from_slice(left);
    data.extend_from_slice(right);
    Sha3_256::digest(&data).into()
}

pub fn compute_new_root(proof: &MerkleProof, new_leaf_hash: Hash) -> Hash {
    let mut current_hash = new_leaf_hash;
    for (is_right, sibling) in &proof.siblings {
        current_hash = if *is_right {
            hash_nodes(&current_hash, sibling)
        } else {
            hash_nodes(sibling, &current_hash)
        };
    }
    current_hash
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MerkleProof {
    pub account: Account,
    pub siblings: Vec<(bool, Hash)>, // saving `is_right` from monotree
}

impl MerkleProof {
    pub fn verify(&self, expected_root: Hash) -> bool {
        let mut current_hash = self.account.hash();
        for (is_right, sibling) in &self.siblings {
            current_hash = if *is_right {
                hash_nodes(&current_hash, sibling)
            } else {
                hash_nodes(sibling, &current_hash)
            };
        }
        current_hash == expected_root
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Deposit {
    pub to: Hash,
    pub amount: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ForcedTx {
    pub from: Hash,
    pub to: Hash,
    pub nonce: u64,
    pub fee: u64,
    pub amount: u64,
    pub timestamp: u64,
    pub l2_calldata: Vec<u8>,
}

pub struct StateTransitioner;

impl StateTransitioner {
    #[allow(clippy::too_many_arguments)]
    pub fn execute_transition(
        pre_state_root: Hash,
        txs: Vec<Transaction>,
        deposits: Vec<Deposit>,
        forced_txs: Vec<ForcedTx>,
        proofs_deposits: Vec<MerkleProof>,
        proofs_forced_from: Vec<MerkleProof>,
        proofs_forced_to: Vec<MerkleProof>,
        proofs_txs_from: Vec<MerkleProof>,
        proofs_txs_to: Vec<MerkleProof>,
    ) -> Result<(Hash, Hash, Hash), StfError> {
        let mut current_root = pre_state_root;

        let deposits_hash = hash_deposits(&deposits);
        let forced_txs_hash = hash_forced_txs(&forced_txs);

        for (i, dep) in deposits.iter().enumerate() {
            let proof_to = &proofs_deposits[i];
            if !proof_to.verify(current_root) {
                return Err(StfError::MerkleProofVerificationFailed);
            }

            let updated_account = apply_deposit(proof_to.account.clone(), dep)?;
            current_root = compute_new_root(proof_to, updated_account.hash());
        }

        for (i, tx) in forced_txs.iter().enumerate() {
            if tx.from == Hash::default() {
                continue;
            }

            let proof_from = &proofs_forced_from[i];
            if !proof_from.verify(current_root) {
                return Err(StfError::MerkleProofVerificationFailed);
            }

            match apply_forced_tx(proof_from.account.clone(), Account::new(tx.to), tx)? {
                TransactionResult::Success(updated_from, updated_to) => {
                    current_root = compute_new_root(proof_from, updated_from.hash());
                    let proof_to = &proofs_forced_to[i];
                    if !proof_to.verify(current_root) {
                        return Err(StfError::MerkleProofVerificationFailed);
                    }
                    current_root = compute_new_root(proof_to, updated_to.hash());
                }
                TransactionResult::Failure(updated_from) => {
                    current_root = compute_new_root(proof_from, updated_from.hash());
                }
            }
        }

        for (i, tx) in txs.iter().enumerate() {
            let proof_from = &proofs_txs_from[i];
            if !proof_from.verify(current_root) {
                return Err(StfError::MerkleProofVerificationFailed);
            }

            match apply_transaction(proof_from.account.clone(), Account::new(tx.to), tx)? {
                TransactionResult::Success(updated_from, updated_to) => {
                    current_root = compute_new_root(proof_from, updated_from.hash());
                    let proof_to = &proofs_txs_to[i];
                    if !proof_to.verify(current_root) {
                        return Err(StfError::MerkleProofVerificationFailed);
                    }
                    current_root = compute_new_root(proof_to, updated_to.hash());
                }
                TransactionResult::Failure(updated_from) => {
                    current_root = compute_new_root(proof_from, updated_from.hash());
                }
            }
        }

        Ok((current_root, deposits_hash, forced_txs_hash))
    }
}

pub fn build_commitment(hash: &Hash, count: u32) -> Hash {
    let mut commitment = [0u8; 32];
    commitment[..28].copy_from_slice(&hash[..28]);
    commitment[28..].copy_from_slice(&count.to_be_bytes());
    commitment
}

sol! {
    struct PublicValues {
        bytes32 preStateRoot;
        bytes32 postStateRoot;
        bytes32 depositsCommitment;
        bytes32 forcedTxsCommitment;
    }

   struct DepositL1 {
        address user;
        uint256 amount;
        uint256 timestamp;
    }

    struct ForcedTransactionL1 {
        bytes data;
        uint256 timestamp;
    }
}

pub fn hash_deposits(items: &[Deposit]) -> Hash {
    if items.is_empty() {
        let mut hasher = Keccak256::new();
        hasher.update(b"");
        return hasher.finalize().into();
    }

    let mut item_hashes = Vec::with_capacity(items.len() * 32);

    for item in items {
        let user_address = Address::from_slice(&item.to[12..]);
        let amount_u256 = U256::from(item.amount);
        let timestamp_u256 = U256::from(item.timestamp);

        let sol_struct = DepositL1 {
            user: user_address,
            amount: amount_u256,
            timestamp: timestamp_u256,
        };

        let encoded_struct = sol_struct.abi_encode();
        let item_hash = Keccak256::digest(&encoded_struct);

        item_hashes.extend_from_slice(&item_hash);
    }

    Keccak256::digest(&item_hashes).into()
}

pub fn hash_forced_txs(items: &[ForcedTx]) -> Hash {
    if items.is_empty() {
        let mut hasher = Keccak256::new();
        hasher.update(b"");
        return hasher.finalize().into();
    }

    let mut item_hashes = Vec::with_capacity(items.len() * 32);

    for item in items {
        let data_bytes = Bytes::from(item.l2_calldata.clone());
        let timestamp_u256 = U256::from(item.timestamp);

        let sol_struct = ForcedTransactionL1 {
            data: data_bytes,
            timestamp: timestamp_u256,
        };

        let encoded_struct = sol_struct.abi_encode();
        let item_hash = Keccak256::digest(&encoded_struct);

        item_hashes.extend_from_slice(&item_hash);
    }

    Keccak256::digest(&item_hashes).into()
}

pub fn apply_deposit(mut account: Account, deposit: &Deposit) -> Result<Account, StfError> {
    account.balance = account
        .balance
        .checked_add(deposit.amount)
        .ok_or(StfError::BalanceOverflow)?;
    Ok(account)
}

pub fn apply_transaction(
    mut from: Account,
    mut to: Account,
    tx: &Transaction,
) -> Result<TransactionResult, StfError> {
    if from.nonce != tx.nonce {
        return Err(StfError::InvalidNonce {
            expected: from.nonce,
            got: tx.nonce,
        });
    }
    from.nonce += 1;

    let total_debit = tx
        .amount
        .checked_add(tx.fee)
        .ok_or(StfError::BalanceOverflow)?;

    if from.balance < total_debit {
        from.balance = from.balance.saturating_sub(tx.fee);
        return Ok(TransactionResult::Failure(from));
    }

    from.balance -= total_debit;
    to.balance = to
        .balance
        .checked_add(tx.amount)
        .ok_or(StfError::BalanceOverflow)?;

    Ok(TransactionResult::Success(from, to))
}

pub fn apply_forced_tx(
    mut from: Account,
    mut to: Account,
    tx: &ForcedTx,
) -> Result<TransactionResult, StfError> {
    if from.nonce != tx.nonce {
        return Err(StfError::InvalidNonce {
            expected: from.nonce,
            got: tx.nonce,
        });
    }
    from.nonce += 1;

    let total_debit = tx
        .amount
        .checked_add(tx.fee)
        .ok_or(StfError::BalanceOverflow)?;

    if from.balance < total_debit {
        from.balance = from.balance.saturating_sub(tx.fee);
        return Ok(TransactionResult::Failure(from));
    }

    from.balance -= total_debit;
    to.balance = to
        .balance
        .checked_add(tx.amount)
        .ok_or(StfError::BalanceOverflow)?;

    Ok(TransactionResult::Success(from, to))
}

#[cfg(test)]
mod tests;
