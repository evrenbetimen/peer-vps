//! Proof of Compute Execution.
//!
//! A renter (or a validator acting for the network) challenges a host to prove
//! it is really running the agreed workload at the agreed tier. The host must
//! answer quickly with work that is (a) bound to the workload's measurement and
//! a fresh nonce, so it cannot be precomputed or copied from another VM, and
//! (b) cheap to spot-check.
//!
//! The current [`HashChainProver`] runs a sequential BLAKE3 chain *inside the
//! guest's pinned cores*, publishing a checkpoint every `stride` steps. The
//! verifier recomputes a few randomly chosen segments and checks the elapsed
//! time against the tier's expected throughput. This is a placeholder for a
//! succinct zero-knowledge proof (zkVM receipt over a workload trace) which will
//! implement the same [`ComputeProver`] / [`ComputeVerifier`] traits.

use std::time::{Duration, Instant};

use rand::Rng;
use serde::{Deserialize, Serialize};

use super::VmId;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub vm: VmId,
    pub nonce: [u8; 32],
    /// Hash of the workload image/rootfs the renter expects to be running.
    pub workload_measurement: [u8; 32],
    pub steps: u64,
    pub stride: u64,
    /// Minimum hash-chain steps per second promised by the performance tier.
    pub min_steps_per_sec: u64,
}

impl Challenge {
    pub fn new(vm: VmId, workload_measurement: [u8; 32], steps: u64, stride: u64, min_steps_per_sec: u64) -> Self {
        let mut nonce = [0u8; 32];
        rand::rng().fill(&mut nonce);
        Self { vm, nonce, workload_measurement, steps, stride, min_steps_per_sec }
    }

    fn seed(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"peervps/poce/v1");
        h.update(self.vm.0.as_bytes());
        h.update(&self.nonce);
        h.update(&self.workload_measurement);
        *h.finalize().as_bytes()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeProof {
    pub checkpoints: Vec<[u8; 32]>,
    pub elapsed_ms: u64,
}

pub trait ComputeProver: Send + Sync {
    fn prove(&self, challenge: &Challenge) -> Result<ComputeProof>;
}

pub trait ComputeVerifier: Send + Sync {
    /// `samples` segments are recomputed; returns Ok(()) if the proof holds.
    fn verify(&self, challenge: &Challenge, proof: &ComputeProof, samples: usize) -> Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HashChainProver;

fn run_segment(mut state: [u8; 32], steps: u64) -> [u8; 32] {
    for _ in 0..steps {
        state = *blake3::hash(&state).as_bytes();
    }
    state
}

impl ComputeProver for HashChainProver {
    fn prove(&self, c: &Challenge) -> Result<ComputeProof> {
        if c.stride == 0 || c.steps == 0 || !c.steps.is_multiple_of(c.stride) {
            return Err(Error::Invalid("steps must be a positive multiple of stride".into()));
        }
        let start = Instant::now();
        let mut state = c.seed();
        let mut checkpoints = Vec::with_capacity((c.steps / c.stride) as usize);
        for _ in 0..c.steps / c.stride {
            state = run_segment(state, c.stride);
            checkpoints.push(state);
        }
        Ok(ComputeProof { checkpoints, elapsed_ms: start.elapsed().as_millis() as u64 })
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HashChainVerifier;

impl ComputeVerifier for HashChainVerifier {
    fn verify(&self, c: &Challenge, proof: &ComputeProof, samples: usize) -> Result<()> {
        let segments = (c.steps / c.stride.max(1)) as usize;
        if proof.checkpoints.len() != segments || segments == 0 {
            return Err(Error::Crypto("wrong number of checkpoints".into()));
        }
        // Segment 0 is always checked: it is the only one tied to the seed, so
        // skipping it would let a host replay a chain computed for another nonce.
        let mut rng = rand::rng();
        let picks = std::iter::once(0).chain((1..samples.min(segments)).map(|_| rng.random_range(0..segments)));
        for i in picks {
            let prev = if i == 0 { c.seed() } else { proof.checkpoints[i - 1] };
            if run_segment(prev, c.stride) != proof.checkpoints[i] {
                return Err(Error::Crypto(format!("checkpoint {i} does not verify")));
            }
        }
        // Tier check: the host must have run at least as fast as promised.
        let max_allowed = Duration::from_secs_f64(c.steps as f64 / c.min_steps_per_sec.max(1) as f64);
        if Duration::from_millis(proof.elapsed_ms) > max_allowed {
            return Err(Error::Crypto(format!(
                "too slow: {} ms for {} steps, tier allows {} ms",
                proof.elapsed_ms,
                c.steps,
                max_allowed.as_millis()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honest_proof_verifies_and_tampering_fails() {
        let c = Challenge::new(VmId::new(), [7; 32], 4_000, 100, 1);
        let mut proof = HashChainProver.prove(&c).expect("prove");
        HashChainVerifier.verify(&c, &proof, 40).expect("all segments checked");

        proof.checkpoints[17][0] ^= 1;
        assert!(HashChainVerifier.verify(&c, &proof, 40).is_err());
    }

    #[test]
    fn proof_is_bound_to_nonce() {
        let vm = VmId::new();
        let a = Challenge::new(vm, [1; 32], 1_000, 100, 1);
        let b = Challenge::new(vm, [1; 32], 1_000, 100, 1);
        let proof = HashChainProver.prove(&a).expect("prove");
        assert!(HashChainVerifier.verify(&b, &proof, 10).is_err());
    }
}
