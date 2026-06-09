use stf::Hash;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CoreError {
    #[error("database error: {0}")]
    DatabaseError(String),

    #[error("account not found: {0:?}")]
    AccountNotFound(Hash),

    #[error("account data corrupted: {0:?}")]
    AccountDataCorrupted(Hash),

    #[error("proof not found for key: {0:?}")]
    ProofNotFound(Hash),

    #[error("stf error: {0}")]
    StfError(String),
}
