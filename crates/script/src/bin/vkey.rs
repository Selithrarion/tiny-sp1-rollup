use sp1_sdk::{Elf, HashableKey, ProvingKey, blocking::MockProver, blocking::Prover, include_elf};

/// The ELF (executable and linkable format) file for the Succinct RISC-V zkVM.
const ROLLUP_ELF: Elf = include_elf!("rollup-program");

fn main() {
    let prover = MockProver::new();
    let pk = prover.setup(ROLLUP_ELF).expect("failed to setup elf");
    println!("{}", pk.verifying_key().bytes32());
}
