use serde::{Deserialize, Serialize};
use stf::{Deposit, ForcedTx, Hash, Transaction};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBlock {
    pub block_number: u64,
    pub txs: Vec<Transaction>,
    pub deposits: Vec<Deposit>,
    pub forced_txs: Vec<ForcedTx>,
    pub pre_state_root: Hash,
    pub post_state_root: Hash,
}
