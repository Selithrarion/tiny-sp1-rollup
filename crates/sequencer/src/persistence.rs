use anyhow::{Context, Result, anyhow};
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options};
use std::sync::Arc;

use crate::PendingBlock;

const PENDING_CF_NAME: &str = "pending_blocks";
const FAILED_CF_NAME: &str = "failed_blocks";
const META_CF_NAME: &str = "meta";

#[derive(Debug)]
pub struct BlockQueueStore {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
}

impl BlockQueueStore {
    pub fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db }
    }

    pub fn push_pending(&self, block: &PendingBlock) -> Result<()> {
        let cf = self
            .db
            .cf_handle(PENDING_CF_NAME)
            .context("push_pending pending_cf err")?;
        let key = block.block_number.to_be_bytes();
        let value = bincode::serialize(block).context("push_pending serialize err")?;
        self.db
            .put_cf(&cf, key, value)
            .context("push_pending put_cf err")?;
        Ok(())
    }

    pub fn peek_pending(&self) -> Result<Option<PendingBlock>> {
        let cf = self
            .db
            .cf_handle(PENDING_CF_NAME)
            .context("peek_pending pending_cf err")?;
        let mut iter = self.db.iterator_cf(&cf, rocksdb::IteratorMode::Start);

        match iter.next() {
            Some(Ok((_key, value))) => {
                let block: PendingBlock =
                    bincode::deserialize(&value).context("failed to deserialize block")?;
                Ok(Some(block))
            }
            Some(Err(e)) => Err(anyhow!("peek_pending iterator err: {:?}", e)),
            None => Ok(None),
        }
    }

    pub fn remove_pending(&self, block_number: u64) -> Result<()> {
        let cf = self
            .db
            .cf_handle(PENDING_CF_NAME)
            .context("remove_pending pending_cf err")?;
        let key = block_number.to_be_bytes();
        self.db
            .delete_cf(&cf, key)
            .context("remove_pending delete_cf err")?;
        Ok(())
    }

    pub fn push_failed(&self, block: &PendingBlock) -> Result<()> {
        let cf = self
            .db
            .cf_handle(FAILED_CF_NAME)
            .context("push_failed failed_cf err")?;
        let key = block.block_number.to_be_bytes();
        let value = bincode::serialize(block).context("push_failed serialize err")?;
        self.db
            .put_cf(&cf, key, value)
            .context("push_failed put_cf err")?;
        Ok(())
    }

    pub fn get_block_counter(&self) -> Result<u64> {
        let cf = self
            .db
            .cf_handle(META_CF_NAME)
            .context("get_block_counter meta_cf err")?;
        let key = b"block_counter";
        match self.db.get_cf(&cf, key)? {
            Some(value) => Ok(u64::from_be_bytes(value.try_into().unwrap_or_default())),
            None => Ok(0),
        }
    }

    pub fn set_block_counter(&self, count: u64) -> Result<()> {
        let cf = self
            .db
            .cf_handle(META_CF_NAME)
            .context("set_block_counter meta_cf err")?;
        let key = b"block_counter";
        let value = count.to_be_bytes();
        self.db
            .put_cf(&cf, key, value)
            .context("set_block_counter put_cf err")?;
        Ok(())
    }

    pub fn commit_optimistic_block(
        &self,
        block: &PendingBlock,
        next_block_number: u64,
    ) -> Result<()> {
        let pending_cf = self
            .db
            .cf_handle(PENDING_CF_NAME)
            .context("commit_optimistic_block pending_cf err")?;
        let meta_cf = self
            .db
            .cf_handle(META_CF_NAME)
            .context("commit_optimistic_block meta_cf err")?;

        let block_key = block.block_number.to_be_bytes();
        let block_value =
            bincode::serialize(block).context("commit_optimistic_block serialize err")?;

        let counter_key = b"block_counter";
        let counter_value = next_block_number.to_be_bytes();

        let mut batch = rocksdb::WriteBatch::default();
        batch.put_cf(&pending_cf, block_key, block_value);
        batch.put_cf(&meta_cf, counter_key, counter_value);
        self.db
            .write(batch)
            .context("commit_optimistic_block write_batch err")
    }
}

pub fn open_db(path: &str) -> Result<Arc<DBWithThreadMode<MultiThreaded>>> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);

    let cf_descriptors = vec![
        ColumnFamilyDescriptor::new(rocksdb::DEFAULT_COLUMN_FAMILY_NAME, Options::default()),
        ColumnFamilyDescriptor::new(META_CF_NAME, Options::default()),
        ColumnFamilyDescriptor::new(PENDING_CF_NAME, Options::default()),
        ColumnFamilyDescriptor::new(FAILED_CF_NAME, Options::default()),
    ];

    let db = DBWithThreadMode::open_cf_descriptors(&opts, path, cf_descriptors)
        .context("open_db open_cf_descriptors err")?;

    Ok(Arc::new(db))
}
