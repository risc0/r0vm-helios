use alloy::providers::Provider;
use alloy::{
    network::EthereumWallet, primitives::Address, providers::ProviderBuilder,
    signers::local::PrivateKeySigner, sol,
};
use alloy_primitives::{B256, U256};
use anyhow::{Context, Result};
use helios_consensus_core::consensus_spec::MainnetConsensusSpec;
use helios_ethereum::consensus::Inner;
use helios_ethereum::rpc::http_rpc::HttpRpc;
use helios_ethereum::rpc::ConsensusRpc;
use log::{error, info};
use r0vm_helios_methods::R0VM_HELIOS_GUEST_ELF;
use r0vm_helios_primitives::types::{ContractStorage, ProofInputs};
use r0vm_helios_script::*;
use reqwest::Url;
use risc0_zkvm::{default_prover, ExecutorEnv, ProverOpts, Receipt};
use std::env;
use std::time::Duration;
use tree_hash::TreeHash;

struct R0VMHeliosOperator {
    wallet: EthereumWallet,
    rpc_url: Url,
    contract_address: Address,
    relayer_address: Address,
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract R0VMHelios {
        bytes32 public immutable GENESIS_VALIDATORS_ROOT;
        uint256 public immutable GENESIS_TIME;
        uint256 public immutable SECONDS_PER_SLOT;
        uint256 public immutable SLOTS_PER_PERIOD;
        uint32 public immutable SOURCE_CHAIN_ID;
        uint256 public head;
        mapping(uint256 => bytes32) public syncCommittees;
        mapping(uint256 => bytes32) public executionStateRoots;
        mapping(uint256 => bytes32) public headers;
        mapping(bytes32 => bytes32) public storageValues;
        bytes32 public heliosImageID;
        address public verifier;

        struct StorageSlot {
            bytes32 key;
            bytes32 value;
            address contractAddress;
        }

        struct ProofOutputs {
            bytes32 executionStateRoot;
            bytes32 newHeader;
            bytes32 nextSyncCommitteeHash;
            uint256 newHead;
            bytes32 prevHeader;
            uint256 prevHead;
            bytes32 syncCommitteeHash;
            bytes32 startSyncCommitteeHash;
            StorageSlot[] slots;
        }

        event HeadUpdate(uint256 indexed slot, bytes32 indexed root);
        event SyncCommitteeUpdate(uint256 indexed period, bytes32 indexed root);
        event StorageSlotVerified(uint256 indexed slot, bytes32 indexed key, bytes32 value, address contractAddress);

        function update(bytes calldata seal, bytes calldata journalData, uint256 head) external;
        function getSyncCommitteePeriod(uint256 slot) internal view returns (uint256);
        function getCurrentSlot() internal view returns (uint256);
        function getCurrentEpoch() internal view returns (uint256);
        function computeStorageKey(uint256 blockNumber, address contractAddress, bytes32 slot) public pure returns (bytes32);
        function getStorageSlot(uint256 blockNumber, address contractAddress, bytes32 slot) external view returns (bytes32);
    }
}

impl R0VMHeliosOperator {
    pub async fn new() -> Self {
        dotenv::dotenv().ok();

        let rpc_url = env::var("DEST_RPC_URL")
            .expect("DEST_RPC_URL not set")
            .parse()
            .unwrap();

        let private_key = env::var("PRIVATE_KEY").expect("PRIVATE_KEY not set");
        let contract_address: Address = env::var("CONTRACT_ADDRESS")
            .expect("CONTRACT_ADDRESS not set")
            .parse()
            .unwrap();
        let signer: PrivateKeySigner = private_key.parse().expect("Failed to parse private key");
        let relayer_address = signer.address();
        let wallet = EthereumWallet::from(signer);

        Self {
            wallet,
            rpc_url,
            contract_address,
            relayer_address,
        }
    }

    /// Fetch values and generate an 'update' proof for the R0VM Helios contract.
    async fn request_update(
        &self,
        mut client: Inner<MainnetConsensusSpec, HttpRpc>,
    ) -> Result<Option<Receipt>> {
        // Fetch required values.
        let provider = ProviderBuilder::new().on_http(self.rpc_url.clone());
        let contract = R0VMHelios::new(self.contract_address, &provider);
        let head: u64 = contract
            .head()
            .call()
            .await
            .unwrap()
            .head
            .try_into()
            .unwrap();
        let period: u64 = contract
            .getSyncCommitteePeriod(U256::from(head))
            .call()
            .await
            .unwrap()
            ._0
            .try_into()
            .unwrap();
        let contract_next_sync_committee = contract
            .syncCommittees(U256::from(period + 1))
            .call()
            .await
            .unwrap()
            ._0;

        // Setup client.
        let mut sync_committee_updates = get_updates(&client).await;
        let finality_update = client.rpc.get_finality_update().await.unwrap();

        // Check if contract is up to date
        let latest_block = finality_update.finalized_header().beacon().slot;
        if latest_block <= head {
            info!("Contract is up to date. Nothing to update.");
            return Ok(None);
        }

        // Optimization:
        // Skip processing update inside program if next_sync_committee is already stored in contract.
        // We must still apply the update locally to "sync" the helios client, this is due to
        // next_sync_committee not being stored when the helios client is bootstrapped.
        if !sync_committee_updates.is_empty() {
            let next_sync_committee = B256::from_slice(
                sync_committee_updates[0]
                    .next_sync_committee()
                    .tree_hash_root()
                    .as_ref(),
            );

            if contract_next_sync_committee == next_sync_committee {
                println!("Applying optimization, skipping update");
                let temp_update = sync_committee_updates.remove(0);

                client.verify_update(&temp_update).unwrap(); // Panics if not valid
                client.apply_update(&temp_update);
            }
        }

        // Create program inputs
        let expected_current_slot = client.expected_current_slot();
        let inputs = ProofInputs {
            sync_committee_updates,
            finality_update,
            expected_current_slot,
            store: client.store.clone(),
            genesis_root: client.config.chain.genesis_root,
            forks: client.config.forks.clone(),
            contract_storage_slots: ContractStorage {
                address: todo!(),
                expected_value: todo!(),
                mpt_proof: todo!(),
                storage_slots: todo!(),
            },
        };
        let encoded_proof_inputs = serde_cbor::to_vec(&inputs)?;

        // Generate proof.
        let proof = tokio::task::spawn_blocking(move || {
            let env = ExecutorEnv::builder()
                .write_frame(&encoded_proof_inputs)
                .build()?;
            default_prover().prove_with_opts(env, R0VM_HELIOS_GUEST_ELF, &ProverOpts::groth16())
        })
        .await
        .unwrap()
        .context("proving failed")?;

        info!("Attempting to update to new head block: {:?}", latest_block);
        Ok(Some(proof.receipt))
    }

    /// Relay an update proof to the R0VM Helios contract.
    async fn relay_update(&self, proof: Receipt, head: u64) -> Result<()> {
        let seal = risc0_ethereum_contracts::encode_seal(&proof)?;

        let wallet_filler = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(self.wallet.clone())
            .on_http(self.rpc_url.clone());
        let contract = R0VMHelios::new(self.contract_address, wallet_filler.clone());

        let nonce = wallet_filler
            .get_transaction_count(self.relayer_address)
            .await?;

        // Wait for 3 required confirmations with a timeout of 60 seconds.
        const NUM_CONFIRMATIONS: u64 = 3;
        const TIMEOUT_SECONDS: u64 = 60;
        let receipt = contract
            .update(
                seal.into(),
                proof.journal.bytes.into(),
                head.try_into().unwrap(),
            )
            .nonce(nonce)
            .send()
            .await?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
            .get_receipt()
            .await?;

        // If status is false, it reverted.
        if !receipt.status() {
            error!("Transaction reverted!");
            return Err(anyhow::anyhow!("Transaction reverted!"));
        }

        info!(
            "Successfully updated to new head block! Tx hash: {:?}",
            receipt.transaction_hash
        );

        Ok(())
    }

    /// Start the operator.
    async fn run(&mut self, loop_delay_mins: u64) -> Result<()> {
        info!("Starting R0VM Helios operator");

        loop {
            let provider = ProviderBuilder::new().on_http(self.rpc_url.clone());
            let contract = R0VMHelios::new(self.contract_address, provider);

            // Get the current slot from the contract
            let slot = contract
                .head()
                .call()
                .await
                .unwrap_or_else(|e| {
                    panic!("Failed to get head. Are you sure the R0VMHelios is deployed to address: {:?}? Error: {:?}", self.contract_address, e)
                })
                .head
                .try_into()
                .unwrap();

            // Fetch the checkpoint at that slot
            let checkpoint = get_checkpoint(slot).await;

            // Get the client from the checkpoint
            let client = get_client(checkpoint).await;

            // Request an update
            match self.request_update(client).await {
                Ok(Some(proof)) => {
                    self.relay_update(proof, slot).await?;
                }
                Ok(None) => {
                    // Contract is up to date. Nothing to update.
                }
                Err(e) => {
                    error!("Header range request failed: {}", e);
                }
            };

            info!("Sleeping for {:?} minutes", loop_delay_mins);
            tokio::time::sleep(tokio::time::Duration::from_secs(60 * loop_delay_mins)).await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env::set_var("RUST_LOG", "info");
    dotenv::dotenv().ok();
    env_logger::init();

    let loop_delay_mins = env::var("LOOP_DELAY_MINS")
        .unwrap_or("5".to_string())
        .parse()?;

    let mut operator = R0VMHeliosOperator::new().await;
    loop {
        if let Err(e) = operator.run(loop_delay_mins).await {
            error!("Error running operator: {}", e);
        }
    }
}
