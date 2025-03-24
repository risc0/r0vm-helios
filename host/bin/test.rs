use anyhow::Result;
use clap::{command, Parser};
use helios_ethereum::rpc::ConsensusRpc;
use risc0_zkvm::{default_prover, ExecutorEnv};
use sp1_helios_methods::SP1_HELIOS_GUEST_ELF;
use sp1_helios_primitives::types::ProofInputs;
use sp1_helios_script::{get_checkpoint, get_client, get_latest_checkpoint, get_updates};

#[derive(Parser, Debug, Clone)]
#[command(about = "Get the genesis parameters from a block.")]
pub struct GenesisArgs {
    #[arg(long)]
    pub slot: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = GenesisArgs::parse();

    // Get the current slot from the contract or fetch the latest checkpoint
    let checkpoint = if let Some(slot) = args.slot {
        get_checkpoint(slot).await
    } else {
        get_latest_checkpoint().await
    };

    // Setup client.
    let helios_client = get_client(checkpoint).await;
    let sync_committee_updates = get_updates(&helios_client).await;
    let finality_update = helios_client.rpc.get_finality_update().await.unwrap();

    let expected_current_slot = helios_client.expected_current_slot();
    let inputs = ProofInputs {
        sync_committee_updates,
        finality_update,
        expected_current_slot,
        store: helios_client.store.clone(),
        genesis_root: helios_client.config.chain.genesis_root,
        forks: helios_client.config.forks.clone(),
    };

    // Write the inputs to the VM
    let env = ExecutorEnv::builder()
        .write_frame(&serde_cbor::to_vec(&inputs)?)
        .build()?;

    let info = default_prover().prove(env, SP1_HELIOS_GUEST_ELF)?;
    println!("Execution Report: {:?}", info.stats);

    Ok(())
}
