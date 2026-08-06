//! BEIT cloud heuristic Ising miner (`BEITUnconstrainedSampler` only).
//!
//! Binary `quip-beit-sa` talks to the AWS Marketplace BEIT SaaS through a
//! long-lived Python sidecar. The coordinator session loop lives in
//! `quip-miner-core`; this crate provides the [`BeitSampler`] backend.
//!
//! Energies are always recomputed with
//! [`quip_protocol::scoring::energy_milli`] from decoded spins — BEIT's own
//! objective is never forwarded as the protocol energy.

// Panic discipline for the library only. Integration tests (`tests/`) are a
// separate crate. Unit tests under `cfg(test)` need expect/panic for asserts.
#![deny(clippy::unwrap_used)]
#![deny(clippy::panic)]
#![warn(clippy::expect_used)]
// Unit tests report failure via expect/panic; those macros are denied above.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

pub mod limits;
pub mod transport;

pub use limits::{
    binary_to_spins, count_ising_nonzeros, ising_to_qubo, milli_to_beit, validate_job,
    LimitViolation, PreparedQubo, QuboTerm, DEFAULT_MAX_BILLED_JOBS, MAX_COEFF_MAGNITUDE,
    MAX_NONZERO_COEFFS, MAX_TIMEOUT_SECS, MAX_VAR_SAMPLE_PRODUCT,
};
pub use quip_miner_core::{IsingGraph, SampleParams, SamplerResult};
pub use transport::{
    BeitTransport, DeadTransport, MockTransport, PythonSidecar, ReplayTransport, SampleResponse,
    SidecarRequest, SidecarResponse, SidecarSample, CUSTOMER_KEY_ENV,
};

use quip_miner_core::adapt::AdaptBounds;
use quip_miner_core::{BackendIdentity, Sampler};
use quip_proto::v1::RejectReason;
use quip_protocol::scoring::energy_milli;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use transport::TransportError;

/// Hard cap on variables advertised in Hello / capabilities.
///
/// BEIT's binding constraint is 5000 non-zeros and `vars * samples <= 1e6`,
/// not a pure node count. 5000 is the safe upper advertisement.
const DEFAULT_MAX_NODES: u32 = 5_000;
/// Hard cap on edges advertised in Hello / capabilities.
const DEFAULT_MAX_EDGES: u32 = 5_000;

/// Conservative adapt envelope: keep samples modest (each job is billed).
const BEIT_ADAPT: AdaptBounds = AdaptBounds {
    min_sweeps: 1,
    max_sweeps: 64,
    min_reads: 1,
    max_reads: 64,
    reads_solution_min_factor: 1,
    reads_solution_max_factor: 4,
    reads_solution_floor_factor: 0,
};

/// Backend identity for `quip-beit-sa`.
pub const BEIT_SA_IDENTITY: BackendIdentity = BackendIdentity {
    backend: "beit",
    algorithm: "sa",
    max_nodes: DEFAULT_MAX_NODES,
    max_edges: DEFAULT_MAX_EDGES,
    adapt: BEIT_ADAPT,
};

/// Default timeout (seconds) when the job does not carry `anneal_time_us`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// Configuration for [`BeitSampler`].
#[derive(Clone, Debug)]
pub struct BeitConfig {
    /// Maximum billed sample jobs per process. Default
    /// [`DEFAULT_MAX_BILLED_JOBS`].
    pub max_billed_jobs: u64,
    /// Default BEIT `timeout` (seconds) when `anneal_time_us` is unset.
    pub default_timeout_secs: u64,
}

impl Default for BeitConfig {
    fn default() -> Self {
        Self {
            max_billed_jobs: DEFAULT_MAX_BILLED_JOBS,
            default_timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// BEIT heuristic sampler backend.
pub struct BeitSampler {
    transport: Arc<dyn BeitTransport>,
    config: BeitConfig,
    billed: AtomicU64,
}

impl BeitSampler {
    /// Build a sampler over an arbitrary transport (live sidecar or mock).
    #[must_use]
    pub fn new(transport: Arc<dyn BeitTransport>, config: BeitConfig) -> Self {
        Self {
            transport,
            config,
            billed: AtomicU64::new(0),
        }
    }

    /// Open the live Python sidecar. Used by `--check` and session mode.
    ///
    /// # Errors
    ///
    /// Returns a message suitable for `OpenError` when the key is missing or
    /// the sidecar fails to start / ping.
    pub fn open_live(config: BeitConfig) -> Result<Self, String> {
        let sidecar = PythonSidecar::spawn().map_err(|e| e.0)?;
        Ok(Self::new(Arc::new(sidecar), config))
    }

    /// Number of billed jobs issued by this sampler.
    #[must_use]
    pub fn billed_count(&self) -> u64 {
        self.billed.load(Ordering::Relaxed)
    }

    fn timeout_secs(&self, _params: &SampleParams) -> u64 {
        // SampleParams has no anneal_time_us field in the shared type; the
        // harness maps anneal into sweeps. Use default timeout, capped at BEIT max.
        // num_sweeps is a free hint only (not a wall-clock budget on BEIT).
        let t = self.config.default_timeout_secs;
        t.clamp(1, MAX_TIMEOUT_SECS)
    }

    fn map_transport_err(err: TransportError) -> RejectReason {
        tracing::error!("BEIT transport error: {}", err.0);
        // Sidecar death / protocol failure: treat as temporary overload so the
        // coordinator can reassign, not as a permanent size reject.
        RejectReason::Overloaded
    }

    fn check_cap(&self) -> Result<(), LimitViolation> {
        let billed = self.billed.load(Ordering::Relaxed);
        if billed >= self.config.max_billed_jobs {
            return Err(LimitViolation::BilledJobCap {
                billed,
                cap: self.config.max_billed_jobs,
            });
        }
        Ok(())
    }
}

impl Sampler for BeitSampler {
    fn sample(
        &self,
        graph: &IsingGraph,
        params: &SampleParams,
    ) -> Result<Vec<SamplerResult>, RejectReason> {
        let sample_count = params.num_reads.max(1);
        let qubo = ising_to_qubo(graph);

        if let Err(v) = validate_job(graph, sample_count, &qubo) {
            tracing::warn!("BEIT reject: {v}");
            return Err(v.reject_reason());
        }
        if let Err(v) = self.check_cap() {
            tracing::warn!("BEIT reject: {v}");
            return Err(v.reject_reason());
        }

        let timeout = self.timeout_secs(params);
        let response = self
            .transport
            .sample(&qubo, sample_count, timeout)
            .map_err(Self::map_transport_err)?;

        // Count the job as billed only after a successful transport round-trip.
        self.billed.fetch_add(1, Ordering::Relaxed);

        let mut results = Vec::with_capacity(response.samples.len());
        for s in &response.samples {
            if s.bits.len() != graph.num_nodes() {
                tracing::error!(
                    "BEIT sample length {} != num_nodes {}",
                    s.bits.len(),
                    graph.num_nodes()
                );
                return Err(RejectReason::Malformed);
            }
            let spins = binary_to_spins(&s.bits);
            // Never trust BEIT's energy field — recompute locally.
            if let Some(beit_e) = s.energy {
                let local = energy_milli(&spins, &graph.h, &graph.j, &graph.edges);
                // Log agreement for operators; mismatches are findings, not bugs to hide.
                let local_as_f = local as f64 / 1000.0;
                if (local_as_f - beit_e).abs() > 1e-3 {
                    tracing::warn!(
                        "BEIT objective {beit_e} disagrees with local energy_milli {local} ({} physical)",
                        local_as_f
                    );
                }
            }
            let energy = energy_milli(&spins, &graph.h, &graph.j, &graph.edges);
            results.push(SamplerResult {
                spins,
                energy_milli: energy,
            });
        }

        if results.is_empty() {
            tracing::error!("BEIT returned zero samples");
            return Err(RejectReason::Overloaded);
        }
        Ok(results)
    }

    fn stream_width(&self) -> usize {
        // Cloud SaaS: serialise billed calls. Parallel jobs would multiply cost
        // and fight a single sidecar stdin lock.
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::binary_to_spins;
    use crate::transport::{MockTransport, ReplayTransport, SidecarSample};
    use quip_protocol::scoring::energy_milli;

    fn ferro() -> IsingGraph {
        IsingGraph::new(vec![0.0, 0.0], vec![-1.0], vec![(0, 1)])
    }

    fn params(reads: usize) -> SampleParams {
        SampleParams {
            num_reads: reads,
            num_sweeps: 1,
            seed: 1,
            ..Default::default()
        }
    }

    #[test]
    fn sample_rescores_with_energy_milli() {
        // Ground state of ferro J=-1 is aligned spins: (+1,+1) or (-1,-1), energy -1.0 → -1000 milli.
        let transport = MockTransport {
            samples: vec![vec![1, 1], vec![0, 0]],
        };
        let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
        let g = ferro();
        let results = sampler.sample(&g, &params(2)).expect("sample");
        assert_eq!(results.len(), 2);
        for r in &results {
            let expected = energy_milli(&r.spins, &g.h, &g.j, &g.edges);
            assert_eq!(r.energy_milli, expected);
            assert_eq!(r.energy_milli, -1000);
        }
    }

    #[test]
    fn rejects_over_nonzero_limit() {
        let h = vec![1.0; MAX_NONZERO_COEFFS + 1];
        let g = IsingGraph::new(h, vec![], vec![]);
        let transport = MockTransport {
            samples: vec![vec![0; MAX_NONZERO_COEFFS + 1]],
        };
        let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
        let err = sampler.sample(&g, &params(1)).unwrap_err();
        assert_eq!(err, RejectReason::TooLarge);
        assert_eq!(sampler.billed_count(), 0, "must not bill rejected jobs");
    }

    #[test]
    fn rejects_over_magnitude() {
        let g = IsingGraph::new(vec![500_000.0], vec![], vec![]);
        let transport = MockTransport {
            samples: vec![vec![0]],
        };
        let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
        let err = sampler.sample(&g, &params(1)).unwrap_err();
        assert_eq!(err, RejectReason::TooLarge);
    }

    #[test]
    fn rejects_var_sample_product() {
        let g = IsingGraph::new(vec![0.0; 1001], vec![], vec![]);
        let transport = MockTransport {
            samples: vec![vec![0; 1001]],
        };
        let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
        let err = sampler.sample(&g, &params(1000)).unwrap_err();
        assert_eq!(err, RejectReason::TooLarge);
    }

    #[test]
    fn billed_cap_rejects_further_jobs() {
        let transport = MockTransport {
            samples: vec![vec![0, 1]],
        };
        let cfg = BeitConfig {
            max_billed_jobs: 1,
            ..BeitConfig::default()
        };
        let sampler = BeitSampler::new(Arc::new(transport), cfg);
        let g = ferro();
        sampler.sample(&g, &params(1)).expect("first job");
        let err = sampler.sample(&g, &params(1)).unwrap_err();
        assert_eq!(err, RejectReason::Overloaded);
    }

    #[test]
    fn sidecar_death_returns_error_not_hang() {
        let transport = DeadTransport;
        let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
        let g = ferro();
        let t0 = std::time::Instant::now();
        let err = sampler.sample(&g, &params(1)).unwrap_err();
        assert!(t0.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(err, RejectReason::Overloaded);
    }

    #[test]
    fn replay_protocol_end_to_end() {
        let g = ferro();
        // x=(1,0) → s=(+1,-1), energy = J*s0*s1 = (-1)*(+1)*(-1) = +1 → +1000 milli
        let transport = ReplayTransport::new(vec![Ok(SampleResponse {
            samples: vec![SidecarSample {
                bits: vec![1, 0],
                energy: Some(999.0), // deliberate mismatch; must not be used
            }],
        })]);
        let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
        let results = sampler.sample(&g, &params(1)).expect("replay sample");
        assert_eq!(results[0].spins, vec![1, -1]);
        assert_eq!(results[0].energy_milli, 1000);
        // BEIT energy was wrong; we still reported local rescore.
        assert_ne!(results[0].energy_milli, 999_000);
    }

    #[test]
    fn binary_spins_match_energy_path() {
        let spins = binary_to_spins(&[0, 1]);
        assert_eq!(spins, vec![-1, 1]);
        let g = ferro();
        let e = energy_milli(&spins, &g.h, &g.j, &g.edges);
        // J=-1, s0=-1, s1=+1 → (-1)*(-1)*(+1)=+1 → 1000 milli
        assert_eq!(e, 1000);
    }
}
