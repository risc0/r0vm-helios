use std::time::Duration;

use anyhow::{Context, Result};
use bonsai_sdk::non_blocking::Client;
use log::info;
use r0vm_helios_methods::R0VM_HELIOS_GUEST_ELF;
use risc0_zkvm::compute_image_id;
use risc0_zkvm::Receipt;

const POLLING_INTERVAL: Duration = Duration::from_secs(5);

pub async fn get_proof(input: Vec<u8>) -> Result<Receipt> {
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
            std::thread::sleep(POLLING_INTERVAL);
            continue;
        }
        if res.status == "SUCCEEDED" {
            // Request that Bonsai compress further, to Groth16.
            let snark_session = client.create_snark(session.uuid).await?;
            let snark_receipt_url = loop {
                let res = snark_session.status(&client).await?;
                match res.status.as_str() {
                    "RUNNING" => {
                        std::thread::sleep(POLLING_INTERVAL);
                        continue;
                    }
                    "SUCCEEDED" => {
                        break res.output.with_context(|| {
                            format!(
                            "Bonsai prover workflow [{}] reported success, but provided no receipt",
                            snark_session.uuid
                        )
                        })?;
                    }
                    _ => {
                        anyhow::bail!(
                            "Bonsai prover workflow [{}] exited: {} err: {}",
                            snark_session.uuid,
                            res.status,
                            res.error_msg
                                .unwrap_or("Bonsai workflow missing error_msg".into()),
                        );
                    }
                }
            };

            let stats = res
                .stats
                .expect("API error, missing stats on completed session");
            info!(
                "Bonsai usage: cycles: {} total_cycles: {}",
                stats.cycles, stats.total_cycles
            );

            let receipt_buf = client.download(&snark_receipt_url).await?;
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
