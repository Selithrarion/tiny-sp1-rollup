use crate::error::CoreError;
use crate::get_account;
use anyhow::Result;
use monotree::Monotree;
use monotree::database::rocksdb::RocksDB;
use monotree::hasher::Sha3;
use std::collections::HashMap;
use stf::{Account, Deposit, ForcedTx, Hash, StfError, Transaction, TransactionResult};

type Tree = Monotree<RocksDB, Sha3>;
pub struct BlockExecutor<'a> {
    tree: &'a mut Tree,
    root: Option<Hash>,
    account_cache: HashMap<Hash, Account>,
}

impl<'a> BlockExecutor<'a> {
    pub fn new(tree: &'a mut Tree, root: Option<Hash>) -> Self {
        Self {
            tree,
            root,
            account_cache: HashMap::new(),
        }
    }

    fn get_account_helper(&mut self, address: &Hash) -> Result<Account, CoreError> {
        if let Some(account) = self.account_cache.get(address) {
            Ok(account.clone())
        } else {
            let account = get_account(self.tree, self.root.as_ref(), address)?;
            self.account_cache.insert(*address, account.clone());
            Ok(account)
        }
    }

    pub fn apply_deposit(&mut self, deposit: &Deposit) -> Result<(), CoreError> {
        let to_account = match self.get_account_helper(&deposit.to) {
            Ok(acc) => acc,
            Err(CoreError::AccountNotFound(_)) => Account::new(deposit.to),
            Err(e) => return Err(e),
        };

        match stf::apply_deposit(to_account, deposit) {
            Ok(updated_account) => {
                self.account_cache.insert(deposit.to, updated_account);
            }
            Err(stf::StfError::BalanceOverflow) => {
                println!(
                    "apply_deposit: balance overflow for {:?}. skipping deposit",
                    deposit.to
                );
            }
            Err(_) => unreachable!("apply_deposit can only return balance overflow"),
        }
        Ok(())
    }

    pub fn apply_forced_tx(&mut self, tx: &ForcedTx) -> Result<(), CoreError> {
        if tx.from == Hash::default() {
            // TODO: take fee?
            println!("apply_forced_tx: skipping dummy forced transaction");
            return Ok(());
        }

        let from_account = match self.get_account_helper(&tx.from) {
            Ok(acc) => acc,
            Err(CoreError::AccountNotFound(_)) => {
                println!("apply_forced_tx: sender account not found, skipping tx");
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let to_account = self
            .get_account_helper(&tx.to)
            .unwrap_or_else(|_| Account::new(tx.to));

        match stf::apply_forced_tx(from_account, to_account, tx) {
            Ok(TransactionResult::Success(updated_from, updated_to)) => {
                self.update_cache_success(tx.from, updated_from, tx.to, updated_to)
            }
            Ok(TransactionResult::Failure(updated_from)) => {
                println!("executor: forced_tx failed, fee charged for {:?}", tx.from);
                self.account_cache.insert(tx.from, updated_from);
            }
            Err(e) => self.handle_stf_error(e, tx.from)?,
        }

        Ok(())
    }

    pub fn apply_transaction(&mut self, tx: &Transaction) -> Result<(), CoreError> {
        println!("apply_transaction start for from: {:?}", tx.from);
        let from_account = match self.get_account_helper(&tx.from) {
            Ok(acc) => acc,
            Err(CoreError::AccountNotFound(_)) => {
                println!("apply_transaction: sender account not found, skipping tx");
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let to_account = self
            .get_account_helper(&tx.to)
            .unwrap_or_else(|_| Account::new(tx.to));
        println!("apply_transaction: got to_account: {:?}", to_account);

        match stf::apply_transaction(from_account, to_account, tx) {
            Ok(TransactionResult::Success(updated_from, updated_to)) => {
                println!("apply_transaction: success");
                self.update_cache_success(tx.from, updated_from, tx.to, updated_to)
            }
            Ok(TransactionResult::Failure(updated_from)) => {
                println!(
                    "executor: transaction failed, fee charged for {:?}",
                    tx.from
                );
                println!("apply_transaction: failure");
                self.account_cache.insert(tx.from, updated_from);
            }
            Err(e) => self.handle_stf_error(e, tx.from)?,
        }

        Ok(())
    }

    pub fn commit_block(self) -> Result<Hash, CoreError> {
        if self.account_cache.is_empty() {
            return Ok(self.root.unwrap_or_default());
        }

        let mut keys = Vec::with_capacity(self.account_cache.len());
        let mut leaves = Vec::with_capacity(self.account_cache.len());

        for (address, account) in self.account_cache {
            keys.push(address);
            leaves.push(account.hash());
        }

        // TODO: check inserts and updates method
        let new_root = self
            .tree
            .inserts(self.root.as_ref(), &keys, &leaves)
            .map_err(|e| CoreError::DatabaseError(format!("commit_block inserts err: {:?}", e)))?
            .ok_or_else(|| {
                CoreError::DatabaseError("commit_block inserts returned none err".to_string())
            })?;

        Ok(new_root)
    }

    fn update_cache_success(
        &mut self,
        from_key: Hash,
        from_account: Account,
        to_key: Hash,
        to_account: Account,
    ) {
        if from_key == to_key {
            self.account_cache.insert(from_key, to_account);
        } else {
            self.account_cache.insert(from_key, from_account);
            self.account_cache.insert(to_key, to_account);
        }
    }

    fn handle_stf_error(&mut self, error: StfError, from: Hash) -> Result<(), CoreError> {
        match error {
            StfError::InvalidNonce { expected, got } => {
                println!(
                    "executor: nonce mismatch for {:?}, expected {}, got {}. skipping tx",
                    from, expected, got
                );
            }
            StfError::BalanceOverflow => {
                println!(
                    "executor: balance overflow on recipient, tx failed for {:?}",
                    from
                );
            }
            StfError::InsufficientBalance => {
                // only by apply_deposit
                println!(
                    "executor: stf error - insufficient balance for {:?}. this should be handled by TransactionResult::Failure.",
                    from
                );
            }
            StfError::MerkleProofVerificationFailed => {
                // only by apply_deposit
                println!(
                    "executor: stf error - merkle proof verification failed for {:?}. this should be handled by generate_proofs_for_block.",
                    from
                );
            }
            StfError::InvalidTransaction(msg) => {
                println!(
                    "executor: stf error - invalid transaction for {:?}: {}",
                    from, msg
                );
            }
        }
        Ok(())
    }
}
