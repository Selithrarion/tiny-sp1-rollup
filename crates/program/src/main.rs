#![no_main]
sp1_zkvm::entrypoint!(main);

use alloy_sol_types::SolType;
use stf::{
    Deposit, ForcedTx, MerkleProof, PublicValues, StateTransitioner, Transaction, build_commitment,
};

pub fn main() {
    // TODO: maybe let inputs = sp1_zkvm::io::read::<TransitionInputs>(); ?
    let pre_state_root = sp1_zkvm::io::read::<[u8; 32]>();
    let transactions = sp1_zkvm::io::read::<Vec<Transaction>>();
    let deposits = sp1_zkvm::io::read::<Vec<Deposit>>();
    let forced_transactions = sp1_zkvm::io::read::<Vec<ForcedTx>>();
    let proofs_deposits = sp1_zkvm::io::read::<Vec<MerkleProof>>();
    let proofs_forced_from = sp1_zkvm::io::read::<Vec<MerkleProof>>();
    let proofs_forced_to = sp1_zkvm::io::read::<Vec<MerkleProof>>();
    let proofs_txs_from = sp1_zkvm::io::read::<Vec<MerkleProof>>();
    let proofs_txs_to = sp1_zkvm::io::read::<Vec<MerkleProof>>();

    let deposits_len = deposits.len() as u32;
    let forced_txs_len = forced_transactions.len() as u32;

    let (post_state_root, deposits_hash, forced_txs_hash) = StateTransitioner::execute_transition(
        pre_state_root,
        transactions,
        deposits,
        forced_transactions,
        proofs_deposits,
        proofs_forced_from,
        proofs_forced_to,
        proofs_txs_from,
        proofs_txs_to,
    )
    .expect("main post_state_root err");

    let public_values = PublicValues {
        preStateRoot: pre_state_root.into(),
        postStateRoot: post_state_root.into(),
        depositsCommitment: build_commitment(&deposits_hash, deposits_len).into(),
        forcedTxsCommitment: build_commitment(&forced_txs_hash, forced_txs_len).into(),
    };
    let bytes = PublicValues::abi_encode(&public_values);

    sp1_zkvm::io::commit_slice(&bytes);
}
