pub mod error;
pub mod executor;
pub mod types;
pub mod utils;

use anyhow::Result;
use monotree::database::rocksdb::RocksDB;
use monotree::hasher::Sha3;
use monotree::{Monotree, Proof};
use stf::{Account, Hash, MerkleProof, TransactionResult};

use crate::error::CoreError;

type Tree = Monotree<RocksDB, Sha3>;

pub fn get_proof(tree: &mut Tree, root: &Hash, key: &Hash) -> Result<Proof, CoreError> {
    if root == &Hash::default() {
        return Err(CoreError::ProofNotFound(*key));
    }

    tree.get_merkle_proof(Some(root), key)
        .map_err(|e| CoreError::DatabaseError(format!("get_proof get_merkle_proof err: {:?}", e)))?
        .ok_or(CoreError::ProofNotFound(*key))
}

pub fn get_account(tree: &mut Tree, root: Option<&Hash>, key: &Hash) -> Result<Account, CoreError> {
    println!("get_account start root: {:?}, key: {:?}", root, key);

    if root == Some(&Hash::default()) {
        return Err(CoreError::AccountNotFound(*key));
    }

    let leaf = tree
        .get(root, key) // panicking and not throwing a proper error eh
        .map_err(|e| CoreError::DatabaseError(format!("get_account get_leaf err: {:?}", e)))?
        .ok_or(CoreError::AccountNotFound(*key))?;

    let account = bincode::deserialize(&leaf).map_err(|_| CoreError::AccountDataCorrupted(*key))?;

    Ok(account)
}

pub fn build_stf_proof(tree: &mut Tree, root: &Hash, key: &Hash) -> Result<MerkleProof, CoreError> {
    let account = match get_account(tree, Some(root), key) {
        Ok(acc) => acc,
        Err(CoreError::AccountNotFound(_)) => Account::new(*key), // if account not found, create a default one for the proof
        Err(e) => return Err(e),
    };

    let siblings = match get_proof(tree, root, key) {
        Ok(proof) => proof
            .into_iter()
            .map(|(is_right, bytes)| {
                let hash: Hash = bytes.try_into().expect("monotree sibling must be 32 bytes");
                (is_right, hash)
            })
            .collect(),
        Err(CoreError::ProofNotFound(_)) => vec![], // if proof not found, it's a new account, so siblings are empty
        Err(e) => return Err(e),
    };

    Ok(MerkleProof { account, siblings })
}

pub struct ProofBundle {
    pub proofs_deposits: Vec<MerkleProof>,
    pub proofs_forced_from: Vec<MerkleProof>,
    pub proofs_forced_to: Vec<MerkleProof>,
    pub proofs_txs_from: Vec<MerkleProof>,
    pub proofs_txs_to: Vec<MerkleProof>,
}

pub fn generate_proofs_for_block(
    tree: &mut Tree,
    block: &types::PendingBlock,
) -> Result<ProofBundle, CoreError> {
    let mut current_root = block.pre_state_root;

    let mut proofs_deposits = Vec::new();
    for dep in &block.deposits {
        let proof = build_stf_proof(tree, &current_root, &dep.to)?;
        match stf::apply_deposit(proof.account.clone(), dep) {
            Ok(updated_account) => {
                current_root = stf::compute_new_root(&proof, updated_account.hash());
            }
            Err(stf::StfError::BalanceOverflow) => {
                println!(
                    "generate_proofs_for_block: balance overflow on deposit for {:?}, skipping",
                    dep.to
                );
            }
            Err(_) => unreachable!(),
        }
        proofs_deposits.push(proof);
    }

    let mut proofs_forced_from = Vec::new();
    let mut proofs_forced_to = Vec::new();
    for tx in &block.forced_txs {
        if tx.from == Hash::default() {
            proofs_forced_from.push(MerkleProof {
                account: Account::new(tx.from),
                siblings: vec![],
            });
            proofs_forced_to.push(MerkleProof {
                account: Account::new(tx.to),
                siblings: vec![],
            });
            continue;
        }

        let proof_from = build_stf_proof(tree, &current_root, &tx.from)?;
        let proof_to = build_stf_proof(tree, &current_root, &tx.to)?;

        match stf::apply_forced_tx(proof_from.account.clone(), proof_to.account.clone(), tx)
            .map_err(|e| CoreError::StfError(format!("{:?}", e)))?
        {
            TransactionResult::Success(updated_from, updated_to) => {
                current_root = stf::compute_new_root(&proof_from, updated_from.hash());
                proofs_forced_from.push(proof_from);

                let proof_to_after_from = build_stf_proof(tree, &current_root, &tx.to)?;
                current_root = stf::compute_new_root(&proof_to_after_from, updated_to.hash());
                proofs_forced_to.push(proof_to_after_from);
            }
            TransactionResult::Failure(updated_from) => {
                current_root = stf::compute_new_root(&proof_from, updated_from.hash());
                proofs_forced_from.push(proof_from);
                proofs_forced_to.push(proof_to);
            }
        };
    }

    let mut proofs_txs_from = Vec::new();
    let mut proofs_txs_to = Vec::new();
    for tx in &block.txs {
        if get_account(tree, Some(&current_root), &tx.from).is_err() {
            println!(
                "generate_proofs_for_block: sender account not found, skipping tx and pushing dummy proofs"
            );
            proofs_txs_from.push(MerkleProof {
                account: Account::new(tx.from),
                siblings: vec![],
            });
            proofs_txs_to.push(MerkleProof {
                account: Account::new(tx.to),
                siblings: vec![],
            });
            continue;
        }

        let proof_from = build_stf_proof(tree, &current_root, &tx.from)?;
        let proof_to = build_stf_proof(tree, &current_root, &tx.to)?;

        match stf::apply_transaction(proof_from.account.clone(), proof_to.account.clone(), tx)
            .map_err(|e| CoreError::StfError(format!("{:?}", e)))?
        {
            TransactionResult::Success(updated_from, updated_to) => {
                current_root = stf::compute_new_root(&proof_from, updated_from.hash());
                proofs_txs_from.push(proof_from);

                let proof_to_after_from = build_stf_proof(tree, &current_root, &tx.to)?;
                current_root = stf::compute_new_root(&proof_to_after_from, updated_to.hash());
                proofs_txs_to.push(proof_to_after_from);
            }
            TransactionResult::Failure(updated_from) => {
                current_root = stf::compute_new_root(&proof_from, updated_from.hash());
                proofs_txs_from.push(proof_from);
                proofs_txs_to.push(proof_to);
            }
        }
    }

    assert_eq!(
        current_root, block.post_state_root,
        "fatal: root mismatch after proof generation simulation"
    );

    Ok(ProofBundle {
        proofs_deposits,
        proofs_forced_from,
        proofs_forced_to,
        proofs_txs_from,
        proofs_txs_to,
    })
}
