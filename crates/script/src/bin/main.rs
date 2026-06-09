use anyhow::{Context, Result};

use alloy_sol_types::SolType;
use clap::Parser;
use monotree::Monotree;
use sp1_sdk::{
    Elf, ProvingKey, SP1Stdin,
    blocking::{ProveRequest, Prover, ProverClient},
    include_elf,
};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use stf::PublicValues;
use thiserror::Error;

const ROLLUP_ELF: Elf = include_elf!("rollup-program");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long)]
    execute: bool,

    #[arg(long)]
    prove: bool,

    #[arg(long)]
    block_data: PathBuf,

    #[arg(long, default_value = "../sequencer/db/state_tree_db")]
    db_path: PathBuf,
}

#[derive(Error, Debug)]
pub enum ScriptError {
    #[error("stf error: {0:?}")]
    Stf(stf::StfError),
}

impl From<stf::StfError> for ScriptError {
    fn from(err: stf::StfError) -> Self {
        ScriptError::Stf(err)
    }
}

fn main() -> Result<()> {
    // Setup the logger.
    sp1_sdk::utils::setup_logger();
    dotenv::dotenv().ok();

    // Parse the command line arguments.
    let args = Args::parse();

    if args.execute == args.prove {
        eprintln!("Error: You must specify either --execute or --prove");
        std::process::exit(1);
    }

    // Setup the prover client.
    let client = ProverClient::from_env();

    // Read the block data from the file.
    let file = File::open(&args.block_data)
        .with_context(|| format!("main open block_data file err: {:?}", args.block_data))?;
    let reader = BufReader::new(file);
    let block: rollup_core::types::PendingBlock =
        serde_json::from_reader(reader).context("main from_reader err")?;

    // Connect to the database to generate proofs.
    let mut tree = Monotree::new(args.db_path.to_str().context("main db_path to_str err")?);

    let proofs = rollup_core::generate_proofs_for_block(&mut tree, &block)
        .context("main generate_proofs_for_block err")?;
    println!("main: root match after simulation, proofs generated successfully");

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

    if args.execute {
        // Execute the program
        let (output, report) = client.execute(ROLLUP_ELF, stdin).run()?;
        println!("Program executed successfully.");

        // Read the output.
        let decoded = PublicValues::abi_decode(output.as_slice())?;
        println!("postStateRoot from vm: {:?}", decoded.postStateRoot);

        // Record the number of cycles executed.
        println!("Number of cycles: {}", report.total_instruction_count());
    } else {
        // Setup the program for proving.
        let pk = client.setup(ROLLUP_ELF)?;

        // Generate the proof
        let proof = client.prove(&pk, stdin).run()?;

        println!("Successfully generated proof!");

        // Verify the proof.
        client.verify(&proof, pk.verifying_key(), None)?;
        println!("Successfully verified proof!");
    }

    Ok(())
}
