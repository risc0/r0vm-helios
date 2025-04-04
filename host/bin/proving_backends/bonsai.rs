use std::time::Duration;

use anyhow::Result;
use bonsai_sdk::non_blocking::Client;
use log::info;
use r0vm_helios_methods::R0VM_HELIOS_GUEST_ELF;
use risc0_zkvm::compute_image_id;
use risc0_zkvm::Receipt;

pub async fn get_proof(input: Vec<u8>) -> Result<Receipt> {
    if !(std::env::var("BONSAI_API_URL").is_ok() && std::env::var("BONSAI_API_KEY").is_ok()) {
        return Err(anyhow::anyhow!(
            "BONSAI_API_URL and BONSAI_API_KEY env vars must be set"
        ));
    }

    let client = Client::from_env("2.0.0")?;

    // Compute the image_id, then upload the ELF with the image_id as its key.
    let image_id = hex::encode(compute_image_id(R0VM_HELIOS_GUEST_ELF)?);
    client
        .upload_img(&image_id, R0VM_HELIOS_GUEST_ELF.to_vec())
        .await?;
    let input_id = client.upload_input(input).await?;

    // Start a session running the prover
    let session = client
        .create_session(image_id, input_id, vec![], false)
        .await?;

    info!("Session: {}", session.uuid);

    loop {
        let res = session.status(&client).await?;
        if res.status == "RUNNING" {
            info!(
                "Current status: {} - state: {} - continue polling...",
                res.status,
                res.state.unwrap_or_default()
            );
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        if res.status == "SUCCEEDED" {
            // Download the receipt, containing the output
            let receipt_url = res
                .receipt_url
                .expect("API error, missing receipt on completed session");

            let stats = res
                .stats
                .expect("API error, missing stats on completed session");
            info!(
                "Bonsai usage: cycles: {} total_cycles: {}",
                stats.cycles, stats.total_cycles
            );

            let receipt_buf = client.download(&receipt_url).await?;
            let receipt: Receipt = bincode::deserialize(&receipt_buf)?;
            return Ok(receipt);
        } else {
            anyhow::bail!(
                "Workflow exited: {} - | err: {}",
                res.status,
                res.error_msg.unwrap_or_default()
            );
        }
    }
}
