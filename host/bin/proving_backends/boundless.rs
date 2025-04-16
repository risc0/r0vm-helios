use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use alloy_primitives::utils::parse_ether;
use anyhow::Result;
use boundless_market::{
    client::ClientBuilder,
    contracts::{Input, Offer, Predicate, ProofRequestBuilder, Requirements},
    input::InputBuilder,
    storage::storage_provider_from_env,
};
use core::time;
use log::info;
use r0vm_helios_methods::{R0VM_HELIOS_GUEST_ELF, R0VM_HELIOS_GUEST_ID};
use risc0_zkvm::{
    default_executor, sha::Digestible, Groth16Receipt, Groth16ReceiptVerifierParameters,
    MaybePruned, Receipt, ReceiptClaim,
};
use std::env;
use std::time::Duration;

pub async fn get_proof(input: Vec<u8>) -> Result<Receipt> {
    let boundless_market_address: Address = env::var("BOUNDLESS_MARKET_ADDRESS")
        .expect("BOUNDLESS_MARKET_ADDRESS not set")
        .parse()
        .unwrap();
    let set_verifier_address: Address = env::var("SET_VERIFIER_ADDRESS")
        .expect("SET_VERIFIER_ADDRESS not set")
        .parse()
        .unwrap();
    let private_key: PrivateKeySigner = env::var("BOUNDLESS_PRIVATE_KEY")
        .expect("BOUNDLESS_PRIVATE_KEY not set")
        .parse()
        .unwrap();
    let rpc_url = env::var("BOUNDLESS_RPC_URL")
        .expect("BOUNDLESS_RPC_URL not set")
        .parse()
        .unwrap();

    let lock_stake: String = env::var("LOCK_STAKE")
        .expect("LOCK_STAKE not set")
        .parse()
        .unwrap();
    let ramp_up = env::var("RAMP_UP")
        .expect("RAMP_UP not set")
        .parse()
        .unwrap();
    let min_price_per_mcycle: String = env::var("MIN_PRICE_PER_MCYCLE")
        .expect("MIN_PRICE_PER_MCYCLE not set")
        .parse()
        .unwrap();
    let max_price_per_mcycle: String = env::var("MAX_PRICE_PER_MCYCLE")
        .expect("MAX_PRICE_PER_MCYCLE not set")
        .parse()
        .unwrap();
    let timeout = env::var("TIMEOUT")
        .expect("TIMEOUT not set")
        .parse()
        .unwrap();
    let lock_timeout = env::var("LOCK_TIMEOUT")
        .expect("LOCK_TIMEOUT not set")
        .parse()
        .unwrap();

    // Create a Boundless client from the provided parameters.
    let boundless_client = ClientBuilder::default()
        .with_rpc_url(rpc_url)
        .with_boundless_market_address(boundless_market_address)
        .with_set_verifier_address(set_verifier_address)
        .with_storage_provider(Some(storage_provider_from_env().await.unwrap()))
        .with_private_key(private_key)
        .build()
        .await?;

    info!("Boundless client created");

    // Upload the ELF to the storage provider so that it can be fetched by the market.
    let image_url = boundless_client.upload_image(R0VM_HELIOS_GUEST_ELF).await?;
    info!("Uploaded image to {}", image_url);

    // Encode the input and upload it to the storage provider.
    let input_builder = InputBuilder::new().write_slice(&input);
    let guest_env = input_builder.clone().build_env()?;
    let guest_env_bytes = guest_env.encode()?;

    // Dry run the ELF with the input to get the journal and cycle count.
    // This can be useful to estimate the cost of the proving request.
    // It can also be useful to ensure the guest can be executed correctly and we do not send into
    // the market unprovable proving requests. If you have a different mechanism to get the expected
    // journal and set a price, you can skip this step.
    info!("Starting local pre-flight execution of guest");
    let session_info =
        default_executor().execute(guest_env.try_into().unwrap(), R0VM_HELIOS_GUEST_ELF)?;
    let mcycles_count = session_info
        .segments
        .iter()
        .map(|segment| 1 << segment.po2)
        .sum::<u64>()
        .div_ceil(1_000_000);
    info!(
        "Local execution completed in {:?} cycles",
        &session_info.cycles()
    );
    let journal = session_info.journal;

    // Create a proof request with the image, input, requirements and offer.
    // The ELF (i.e. image) is specified by the image URL.
    // The input can be specified by an URL, as in this example, or can be posted on chain by using
    // the `with_inline` method with the input bytes.
    // The requirements are the image ID and the digest of the journal. In this way, the market can
    // verify that the proof is correct by checking both the committed image id and digest of the
    // journal. The offer specifies the price range and the timeout for the request.
    // Additionally, the offer can also specify:
    // - the bidding start time: the block number when the bidding starts;
    // - the ramp up period: the number of blocks before the price start increasing until reaches
    //   the maxPrice, starting from the the bidding start;
    // - the lockin price: the price at which the request can be locked in by a prover, if the
    //   request is not fulfilled before the timeout, the prover can be slashed.
    // If the input exceeds 2 kB, upload the input and provide its URL instead, as a rule of thumb.
    let request_input = if guest_env_bytes.len() > 2 << 10 {
        let input_url = boundless_client.upload_input(&guest_env_bytes).await?;
        info!("Uploaded input to {}", input_url);
        Input::url(input_url)
    } else {
        info!("Sending input inline with request");
        Input::inline(guest_env_bytes.clone())
    };

    let request = ProofRequestBuilder::new()
        .with_image_url(image_url.to_string())
        .with_input(request_input)
        .with_requirements(
            Requirements::new(
                R0VM_HELIOS_GUEST_ID,
                Predicate::digest_match(journal.digest()),
            )
            .with_groth16_proof(), // For this test ensure no batching so we can use the proof directly
        )
        .with_offer(
            Offer::default()
                .with_lock_stake(parse_ether(&lock_stake)?)
                // The market uses a reverse Dutch auction mechanism to match requests with provers.
                // Each request has a price range that a prover can bid on. One way to set the price
                // is to choose a desired (min and max) price per million cycles and multiply it
                // by the number of cycles. Alternatively, you can use the `with_min_price` and
                // `with_max_price` methods to set the price directly.
                .with_min_price_per_mcycle(parse_ether(&min_price_per_mcycle)?, mcycles_count)
                // NOTE: If your offer is not being accepted, try increasing the max price.
                .with_max_price_per_mcycle(parse_ether(&max_price_per_mcycle)?, mcycles_count)
                // The timeout is the maximum number of blocks the request can stay
                // unfulfilled in the market before it expires. If a prover locks in
                // the request and does not fulfill it before the timeout, the prover can be
                // slashed.
                .with_timeout(timeout)
                // The lock timeout is the maximum number of blocks a prover can lock an order
                // for before being slashed
                .with_lock_timeout(lock_timeout)
                .with_ramp_up_period(ramp_up),
        )
        .build()
        .unwrap();

    // Send the request and wait for it to be completed.
    let (request_id, expires_at) = boundless_client.submit_request(&request).await?;
    info!("Request 0x{request_id:x} submitted");
    info!("Track at https://indexer.beboundless.xyz/orders/0x{request_id:x}");

    // Wait for the request to be fulfilled by the market, returning the journal and seal.
    info!("Waiting for 0x{request_id:x} to be fulfilled");
    let (_journal, seal) = boundless_client
        .wait_for_request_fulfillment(request_id, Duration::from_secs(10), expires_at)
        .await?;
    info!("Request 0x{request_id:x} fulfilled");

    let inner = risc0_zkvm::InnerReceipt::Groth16(Groth16Receipt::new(
        seal.to_vec(),
        MaybePruned::Value(ReceiptClaim::ok(
            R0VM_HELIOS_GUEST_ID,
            journal.bytes.clone(),
        )),
        Groth16ReceiptVerifierParameters::default().digest(),
    ));
    Ok(Receipt::new(inner, journal.as_ref().to_vec()))
}
