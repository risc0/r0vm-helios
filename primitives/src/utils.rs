use alloy_primitives::{keccak256, B256};
use helios_consensus_core::types::Forks;

pub fn compute_chain_commitment(
    genesis_root: B256,
    forks: &Forks,
) -> Result<B256, serde_cbor::Error> {
    Ok(keccak256(
        [genesis_root.as_slice(), &serde_cbor::to_vec(forks)?].concat(),
    ))
}
