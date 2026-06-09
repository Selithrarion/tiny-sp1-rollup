pub mod config;
pub mod persistence;

use crate::TinyRollupBridge::TinyRollupBridgeInstance;
use crate::config::AppConfig;
use alloy::contract::SolCallBuilder;
use alloy::network::EthereumWallet;
use alloy::primitives::U256;
use alloy::providers::fillers::{
    BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
};
use alloy::providers::{Identity, ProviderBuilder, RootProvider};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolType, sol};
use alloy::transports::http::reqwest::Url;
use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router, debug_handler,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use backoff::{ExponentialBackoff, future::retry};
use monotree::Monotree;
use monotree::database::rocksdb::RocksDB;
use monotree::hasher::Sha3;
use rollup_core::executor::BlockExecutor;
use rollup_core::types::PendingBlock;
use sp1_sdk::env::{EnvProver, EnvProvingKey};
use sp1_sdk::{Elf, Prover, ProverClient, SP1Stdin, include_elf};
use std::str::FromStr;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use stf::{Deposit, ForcedTx, Hash, PublicValues, Transaction};
use thiserror::Error;
use tokio::sync::RwLock;
use tokio::task::spawn_blocking;

sol! {
    #[sol(rpc)]
    contract TinyRollupBridge {
        function updateState(bytes calldata _publicValues, bytes calldata _proofBytes) external;

        function depositCount() external view returns (uint256);
        function forcedTxCount() external view returns (uint256);
        function processedDepositCount() external view returns (uint256);
        function processedForcedTxCount() external view returns (uint256);

        struct Deposit { address user; uint256 amount; uint256 timestamp; }
        function getDeposits(uint256 _start, uint256 _count) external view returns (Deposit[] memory);

        struct ForcedTransaction { bytes data; uint256 timestamp; }
        function getForcedTransactions(uint256 _start, uint256 _count) external view returns (ForcedTransaction[] memory);
    }
}

#[derive(Error, Debug, Clone)]
pub enum SequencerError {
    #[error("stf error: {0:?}")]
    Stf(stf::StfError),
    #[error("mempool empty err")]
    MempoolEmpty,
    #[error("account not found: {0:?}")]
    AccountNotFound(Hash),
    #[error("insufficient balance for account: {0:?}")]
    InsufficientBalance(Hash),
    #[error("invalid nonce for account {0:?}: expected {1}, got {2}")]
    InvalidNonce(Hash, u64, u64),
    #[error("l1 submit transaction err")]
    L1SubmitFailed,
    #[error("state lock is poisoned")]
    StateLockPoisoned,
    #[error("account data corrupted for key: {0:?}")]
    AccountDataCorrupted(Hash),
    #[error("database internal error: {0}")]
    DatabaseError(String),
}

impl From<stf::StfError> for SequencerError {
    fn from(err: stf::StfError) -> Self {
        SequencerError::Stf(err)
    }
}

impl From<rollup_core::error::CoreError> for SequencerError {
    fn from(value: rollup_core::error::CoreError) -> Self {
        match value {
            rollup_core::error::CoreError::DatabaseError(s) => SequencerError::DatabaseError(s),
            rollup_core::error::CoreError::AccountNotFound(h) => SequencerError::AccountNotFound(h),
            rollup_core::error::CoreError::AccountDataCorrupted(h) => {
                SequencerError::AccountDataCorrupted(h)
            }
            rollup_core::error::CoreError::ProofNotFound(h) => {
                SequencerError::DatabaseError(format!("proof not found for {:?}", h))
            }
            rollup_core::error::CoreError::StfError(s) => {
                SequencerError::Stf(stf::StfError::InvalidTransaction(s))
            }
        }
    }
}

impl IntoResponse for SequencerError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            SequencerError::MempoolEmpty => (StatusCode::BAD_REQUEST, self.to_string()),
            SequencerError::AccountNotFound(_) => (StatusCode::NOT_FOUND, self.to_string()),
            SequencerError::InsufficientBalance(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            SequencerError::InvalidNonce(_, _, _) => (StatusCode::BAD_REQUEST, self.to_string()),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
        };

        let body = Json(serde_json::json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

type Tree = Monotree<RocksDB, Sha3>;

pub struct OffchainState {
    pub heads: RwLock<Heads>,
}

impl OffchainState {
    fn new(initial_root: Hash) -> Self {
        Self {
            heads: RwLock::new(Heads {
                proven: initial_root,
                optimistic: initial_root,
            }),
        }
    }
}

const ROLLUP_ELF: Elf = include_elf!("rollup-program");

#[derive(Debug)]
pub struct Heads {
    pub proven: Hash,
    pub optimistic: Hash,
}

pub struct AppState {
    pub mempool: RwLock<VecDeque<Transaction>>,
    pub state: Arc<OffchainState>,
    pub tree: Arc<Mutex<Tree>>,
    pub block_counter: AtomicU64,
    pub l1_bridge: TinyRollupBridgeInstance<
        FillProvider<
            JoinFill<
                Identity,
                JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
            >,
            RootProvider,
        >,
    >,
    pub config: AppConfig,
    pub prover_client: EnvProver,
    pub proving_key: EnvProvingKey,
    pub block_queue_store: persistence::BlockQueueStore,
}

#[tokio::main]
async fn main() -> Result<()> {
    unsafe {
        std::env::set_var("SP1_PROVER", "mock");
    }
    sp1_sdk::utils::setup_logger();

    let config = AppConfig::load().context("main load_config err")?;

    let prover_client = ProverClient::from_env().await;
    let proving_key = prover_client
        .setup(ROLLUP_ELF)
        .await
        .context("main setup_prover err")?;

    let mut tree = Tree::new(&config.database.state_tree_path);
    if tree.get_headroot()?.is_none() {
        let empty_root = Hash::default();
        tree.set_headroot(Some(&empty_root));
    }
    let initial_root = tree.get_headroot()?.context("main get_headroot err")?;

    println!("config {:?}", config);
    println!("connecting to {}", config.l1.rpc_url);
    println!(
        "connecting to {} (parsed)",
        config.l1.rpc_url.parse::<Url>()?
    );
    let provider = ProviderBuilder::new().connect_http(config.l1.rpc_url.parse::<Url>()?);
    let l1_bridge = TinyRollupBridge::new(config.l1.bridge_address, provider);

    let queue_db = persistence::open_db(&config.database.queues_path)?;
    let block_queue_store = persistence::BlockQueueStore::new(queue_db);

    let initial_block_counter = block_queue_store.get_block_counter()?;
    println!("restored block counter: {}", initial_block_counter);

    let app_state = Arc::new(AppState {
        mempool: RwLock::new(VecDeque::new()),
        state: Arc::new(OffchainState::new(initial_root)),
        tree: Arc::new(Mutex::new(tree)),
        block_counter: AtomicU64::new(initial_block_counter),
        l1_bridge,
        config,
        prover_client,
        proving_key,
        block_queue_store,
    });

    let state_clone_optimistic = app_state.clone();
    tokio::spawn(async move {
        if let Err(e) = optimistic_worker_loop(state_clone_optimistic).await {
            eprintln!("optimistic worker crashed: {:?}", e);
        }
    });

    let state_clone = app_state.clone();
    tokio::spawn(async move {
        if let Err(e) = prover_worker_loop(state_clone).await {
            eprintln!("prover worker crashed: {:?}", e);
        }
    });

    let app = Router::new()
        .route("/tx", post(handle_submit_tx))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .context("main bind_listener err")?;
    println!("sequencer API started on port 3000");
    axum::serve(listener, app).await?;

    Ok(())
}

#[debug_handler]
async fn handle_submit_tx(
    State(state): State<Arc<AppState>>,
    Json(tx): Json<Transaction>,
) -> Result<Json<String>, SequencerError> {
    // TODO: add signature+nonce validation
    let mut mempool = state.mempool.write().await;
    mempool.push_back(tx);

    println!("tx accepted to mempool");
    Ok(Json("tx accepted".to_string()))
}

async fn optimistic_worker_loop(state: Arc<AppState>) -> Result<()> {
    println!("optimistic_worker_loop: start");
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;

        let (deposits, forced_txs) = if state.config.l1.enabled {
            let (deposits_res, forced_txs_res) = tokio::join!(
                fetch_new_deposits(state.clone()),
                fetch_new_forced_txs(state.clone())
            );

            let deposits = deposits_res.context("optimistic_worker_loop fetch_new_deposits err")?;
            let forced_txs =
                forced_txs_res.context("optimistic_worker_loop fetch_new_forced_txs err")?;
            (deposits, forced_txs)
        } else {
            (Vec::new(), Vec::new())
        };

        let txs = {
            let mempool = state.mempool.read().await;
            if mempool.is_empty() && deposits.is_empty() && forced_txs.is_empty() {
                continue;
            }
            mempool.iter().cloned().collect::<Vec<Transaction>>()
        };

        let pre_root = state.state.heads.read().await.optimistic;

        let state_clone = state.clone();
        let txs_clone = txs.clone();
        let deposits_clone = deposits.clone();
        let forced_txs_clone = forced_txs.clone();
        let (block_number, new_optimistic_root) = spawn_blocking(move || -> Result<_> {
            let mut tree = state_clone
                .tree
                .lock()
                .map_err(|_| SequencerError::StateLockPoisoned)?;
            println!(
                "applying optimistic block with {} txs, {} deposits, {} forced_txs",
                txs_clone.len(),
                deposits_clone.len(),
                forced_txs_clone.len()
            );

            let new_optimistic_root = apply_optimistic_batch(
                &mut tree,
                Some(pre_root),
                &txs_clone,
                &deposits_clone,
                &forced_txs_clone,
            )?;

            println!("bef  set_headroot");
            tree.set_headroot(Some(&new_optimistic_root));

            let block_number = state_clone.block_counter.fetch_add(1, Ordering::Relaxed);
            Ok((block_number, new_optimistic_root))
        })
        .await
        .context("optimistic_worker_loop spawn_blocking err")??;

        state.state.heads.write().await.optimistic = new_optimistic_root;

        {
            let mut mempool = state.mempool.write().await;
            mempool.drain(..txs.len());
        }

        let block = PendingBlock {
            block_number,
            txs,
            deposits,
            forced_txs,
            pre_state_root: pre_root,
            post_state_root: new_optimistic_root,
        };

        state
            .block_queue_store
            .commit_optimistic_block(&block, block_number + 1)?;
        println!(
            "optimistic block #{} created. new root: {:?}",
            block_number, new_optimistic_root
        );
    }
}

async fn fetch_new_deposits(state: Arc<AppState>) -> Result<Vec<Deposit>> {
    let backoff_strategy = ExponentialBackoff::default();

    let operation = || async {
        state
            .l1_bridge
            .processedDepositCount()
            .call()
            .await
            .map_err(backoff::Error::transient)
    };
    let processed_deposit_count = retry(backoff_strategy.clone(), operation)
        .await
        .context("fetch_new_deposits processed_deposit_count err")?
        .to::<u64>();

    let operation = || async {
        state
            .l1_bridge
            .depositCount()
            .call()
            .await
            .map_err(backoff::Error::transient)
    };
    let l1_deposit_count = retry(backoff_strategy.clone(), operation)
        .await
        .context("fetch_new_deposits deposit_count err")?
        .to::<u64>();

    let mut new_deposits = Vec::new();
    if l1_deposit_count > processed_deposit_count {
        let num_to_fetch = l1_deposit_count - processed_deposit_count;
        println!("fetch_new_deposits: fetching {} new deposits", num_to_fetch);

        let operation = || async {
            state
                .l1_bridge
                .getDeposits(
                    U256::from(processed_deposit_count),
                    U256::from(num_to_fetch),
                )
                .call()
                .await
                .map_err(backoff::Error::transient)
        };
        let deposits_from_l1 = retry(backoff_strategy, operation)
            .await
            .context("fetch_new_deposits get_deposits err")?;

        for d in deposits_from_l1 {
            new_deposits.push(Deposit {
                to: rollup_core::utils::address_to_hash(&d.user),
                amount: d.amount.to::<u64>(),
                timestamp: d.timestamp.to::<u64>(),
            });
        }
    }
    Ok(new_deposits)
}

async fn fetch_new_forced_txs(state: Arc<AppState>) -> Result<Vec<ForcedTx>> {
    let backoff_strategy = ExponentialBackoff::default();

    let operation = || async {
        state
            .l1_bridge
            .processedForcedTxCount()
            .call()
            .await
            .map_err(backoff::Error::transient)
    };
    let processed_forced_tx_count = retry(backoff_strategy.clone(), operation)
        .await
        .context("fetch_new_forced_txs processed_forced_tx_count err")?
        .to::<u64>();

    let operation = || async {
        state
            .l1_bridge
            .forcedTxCount()
            .call()
            .await
            .map_err(backoff::Error::transient)
    };
    let l1_forced_tx_count = retry(backoff_strategy.clone(), operation)
        .await
        .context("fetch_new_forced_txs forced_tx_count err")?
        .to::<u64>();

    let mut new_forced_txs = Vec::new();
    if l1_forced_tx_count > processed_forced_tx_count {
        let num_to_fetch = l1_forced_tx_count - processed_forced_tx_count;
        println!(
            "fetch_new_forced_txs: fetching {} new forced txs",
            num_to_fetch
        );

        let operation = || async {
            state
                .l1_bridge
                .getForcedTransactions(
                    U256::from(processed_forced_tx_count),
                    U256::from(num_to_fetch),
                )
                .call()
                .await
                .map_err(backoff::Error::transient)
        };
        let forced_txs_from_l1 = retry(backoff_strategy, operation)
            .await
            .context("fetch_new_forced_txs get_forced_transactions err")?;

        for l1_tx in forced_txs_from_l1 {
            let timestamp_u64 = l1_tx.timestamp.to::<u64>();

            match bincode::deserialize::<ForcedTx>(&l1_tx.data) {
                Ok(mut tx) => {
                    tx.timestamp = timestamp_u64;
                    tx.l2_calldata = l1_tx.data.to_vec();
                    new_forced_txs.push(tx);
                }
                Err(e) => {
                    eprintln!(
                        "fetch_new_forced_txs: failed to deserialize forced tx data: {:?}. creating failed dummy tx",
                        e
                    );

                    let dummy_tx = ForcedTx {
                        from: Hash::default(),
                        to: Hash::default(),
                        nonce: 0,
                        fee: 0,
                        amount: 0,
                        timestamp: timestamp_u64,
                        l2_calldata: l1_tx.data.to_vec(),
                    };
                    new_forced_txs.push(dummy_tx);
                }
            }
        }
    }
    Ok(new_forced_txs)
}

async fn prover_worker_loop(state: Arc<AppState>) -> Result<()> {
    const MAX_RETRIES: u32 = 3;

    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;

        let block_to_prove = state.block_queue_store.peek_pending()?; // TODO: maybe can bench if spawn_blocked needed for these tiny io ops
        let block = match block_to_prove {
            Some(b) => b,
            None => continue,
        };
        println!("proving block #{}", block.block_number);

        let mut retries = 0;
        loop {
            match process_and_prove_batch(&state, block.clone()).await {
                Ok(_) => {
                    println!(
                        "block #{} proven and sent to l1 successfully",
                        block.block_number
                    );
                    let remove_op = || async {
                        state.block_queue_store.remove_pending(block.block_number)
                            .map_err(|e| {
                                eprintln!("critical: failed to remove proven block #{}: {:?}. retrying...", block.block_number, e);
                                backoff::Error::transient(e)
                            })
                    };
                    retry(ExponentialBackoff::default(), remove_op).await?;
                    break;
                }
                Err(e) => {
                    retries += 1;
                    eprintln!(
                        "failed to prove block #{}: {:?}. retry {}/{}",
                        block.block_number, e, retries, MAX_RETRIES
                    );
                    if retries >= MAX_RETRIES {
                        let push_failed_op = || async {
                            state.block_queue_store.push_failed(&block)
                                .map_err(|e| {
                                    eprintln!("critical: failed to move block #{} to failed queue: {:?}. retrying...", block.block_number, e);
                                    backoff::Error::transient(e)
                                })
                        };
                        retry(ExponentialBackoff::default(), push_failed_op).await?;

                        let remove_op = || async {
                            state.block_queue_store.remove_pending(block.block_number)
                                .map_err(|e| {
                                    eprintln!("critical: failed to remove block #{} from pending after moving to failed: {:?}. retrying...", block.block_number, e);
                                    backoff::Error::transient(e)
                                })
                        };
                        retry(ExponentialBackoff::default(), remove_op).await?;

                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }
}

// assuming already validated invalid txs
// maybe have to recheck and remove smth from optimistic fn
// or write more comments what we're validating and what we're not
// kinda getting lost already
async fn process_and_prove_batch(state: &Arc<AppState>, block: PendingBlock) -> Result<()> {
    let state_clone = state.clone();
    let block_clone = block.clone();
    let proofs = spawn_blocking(move || -> Result<_> {
        println!(
            "process_and_prove_batch: start proof generation for block #{}",
            block_clone.block_number
        );
        let mut tree = state_clone
            .tree
            .lock()
            .map_err(|_| SequencerError::StateLockPoisoned)?;
        let proofs = rollup_core::generate_proofs_for_block(&mut tree, &block_clone)?;
        Ok(proofs)
    })
    .await??;

    let mut stdin = SP1Stdin::new();
    stdin.write(&block.pre_state_root);
    stdin.write(&block.txs);
    stdin.write(&block.deposits);
    stdin.write(&block.forced_txs);
    stdin.write(&proofs.proofs_deposits);
    stdin.write(&proofs.proofs_forced_from);
    stdin.write(&proofs.proofs_forced_to);
    stdin.write(&proofs.proofs_txs_from);
    stdin.write(&proofs.proofs_txs_to);

    println!("generating proof...");
    let proof = state
        .prover_client
        .prove(&state.proving_key, stdin)
        .await
        .context("process_and_prove_batch prove err")?;
    println!("proof generated successfully");

    let decoded_pv = <PublicValues as SolType>::abi_decode(proof.public_values.as_slice())?;
    let post_state_root: [u8; 32] = decoded_pv.postStateRoot.into();
    if post_state_root != block.post_state_root {
        return Err(anyhow!("process_and_prove_batch root mismatch"));
    }

    if state.config.l1.enabled {
        submit_proof_to_l1(state, proof.public_values.to_vec(), proof.bytes())
            .await
            .context("process_and_prove_batch submit_proof err")?;
    } else {
        println!("process_and_prove_batch: l1 is disabled, skipping proof submission");
    }

    state.state.heads.write().await.proven = post_state_root;

    Ok(())
}

async fn submit_proof_to_l1(
    state: &Arc<AppState>,
    public_values: Vec<u8>,
    proof: Vec<u8>,
) -> Result<()> {
    let signer = PrivateKeySigner::from_str(&state.config.l1.private_key)
        .context("submit_proof_to_l1 from_str err")?;
    let wallet = EthereumWallet::from(signer);

    // TODO: do not create new but reuse?
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(state.config.l1.rpc_url.parse()?);

    let contract = TinyRollupBridge::new(*state.l1_bridge.address(), provider);

    let public_values_bytes = public_values.into();
    let proof_bytes = proof.into();

    println!("submit_proof_to_l1: sending zk proof update transaction to l1...");
    let tx_builder: SolCallBuilder<_, _> = contract.updateState(public_values_bytes, proof_bytes);
    let pending_tx = tx_builder.send().await?;
    println!("tx sent, hash: {}", pending_tx.tx_hash());

    let receipt = pending_tx
        .get_receipt()
        .await
        .context("submit_proof_to_l1 get_receipt err")?; // TODO: do we need to use 2/3 quorum here?
    if !receipt.status() {
        return Err(anyhow!("submit_proof_to_l1 tx reverted"));
    }
    println!(
        "submit_proof_to_l1: l1 transaction confirmed in block {}",
        receipt.block_number.unwrap_or_default()
    );
    Ok(())
}

fn apply_optimistic_batch(
    tree: &mut Tree,
    root: Option<Hash>,
    txs: &[Transaction],
    deposits: &[Deposit],
    forced_txs: &[ForcedTx],
) -> Result<Hash, SequencerError> {
    println!("apply_optimistic_batch start");

    if txs.is_empty() && deposits.is_empty() && forced_txs.is_empty() {
        return Ok(root.unwrap_or_default());
    }

    let mut executor = BlockExecutor::new(tree, root);

    for deposit in deposits {
        executor.apply_deposit(deposit)?;
    }
    for forced_tx in forced_txs {
        executor.apply_forced_tx(forced_tx)?;
    }
    for tx in txs {
        executor.apply_transaction(tx)?;
    }

    Ok(executor.commit_block()?)
}
