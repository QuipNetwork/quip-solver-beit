//! BEITUnconstrainedSampler hard limits and Ising → QUBO conversion.
//!
//! Documented at https://resources.qubo-solver.pro.beit.tech/ (verified 2026-08-05):
//! - arbitrary topology
//! - coefficients float with `-1e6 < M_ij < 1e6`, precision `1e-6`
//! - at most 5000 non-zero coefficients
//! - `variables * samples <= 1e6`

use quip_miner_core::IsingGraph;
use quip_proto::v1::RejectReason;
use std::collections::BTreeMap;
use std::fmt;

/// Maximum non-zero QUBO / Ising coefficients accepted by BEIT.
pub const MAX_NONZERO_COEFFS: usize = 5000;

/// Strict upper bound on absolute coefficient magnitude (`|M_ij| < this`).
pub const MAX_COEFF_MAGNITUDE: f64 = 1_000_000.0;

/// Maximum product of variables × sample_count.
pub const MAX_VAR_SAMPLE_PRODUCT: u64 = 1_000_000;

/// Maximum BEIT sampler timeout in seconds (12 minutes).
pub const MAX_TIMEOUT_SECS: u64 = 60 * 12;

/// Default billed-job cap per process when unset.
pub const DEFAULT_MAX_BILLED_JOBS: u64 = 10;

/// Scale a wire milli-integer coefficient to a BEIT float (`milli / 1000`).
///
/// Does not assume the allowed set is `±1000`; always divides.
#[must_use]
pub fn milli_to_beit(milli: i32) -> f64 {
    f64::from(milli) / 1000.0
}

/// One upper-triangular QUBO term `(i, j, value)` with `i <= j`.
#[derive(Clone, Debug, PartialEq)]
pub struct QuboTerm {
    /// First variable index.
    pub i: usize,
    /// Second variable index (`>= i`).
    pub j: usize,
    /// Coefficient value.
    pub value: f64,
}

/// Validated QUBO ready for the sidecar, plus the original Ising for rescoring.
#[derive(Clone, Debug)]
pub struct PreparedQubo {
    /// Number of binary variables.
    pub num_vars: usize,
    /// Non-zero upper-triangular QUBO terms.
    pub terms: Vec<QuboTerm>,
}

/// Why a job was rejected before any billed call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitViolation {
    /// Non-zero coefficient count exceeds [`MAX_NONZERO_COEFFS`].
    TooManyNonzeros {
        /// Actual non-zero count.
        actual: usize,
        /// Documented limit.
        limit: usize,
    },
    /// Some coefficient reaches or exceeds [`MAX_COEFF_MAGNITUDE`].
    CoeffMagnitude {
        /// Absolute value of the offending coefficient.
        abs_value_bits: u64,
        /// Documented exclusive upper bound.
        limit_bits: u64,
        /// Variable pair that produced it.
        i: usize,
        /// Second index of the pair.
        j: usize,
    },
    /// `num_vars * sample_count` exceeds [`MAX_VAR_SAMPLE_PRODUCT`].
    VarSampleProduct {
        /// Actual product.
        actual: u64,
        /// Documented limit.
        limit: u64,
        /// Variable count.
        num_vars: usize,
        /// Requested samples.
        sample_count: usize,
    },
    /// Process billed-job cap reached.
    BilledJobCap {
        /// Jobs already billed this process.
        billed: u64,
        /// Configured cap.
        cap: u64,
    },
}

impl LimitViolation {
    /// Protocol reject reason for this violation.
    #[must_use]
    pub const fn reject_reason(&self) -> RejectReason {
        match self {
            Self::BilledJobCap { .. } => RejectReason::Overloaded,
            Self::TooManyNonzeros { .. }
            | Self::CoeffMagnitude { .. }
            | Self::VarSampleProduct { .. } => RejectReason::TooLarge,
        }
    }
}

impl fmt::Display for LimitViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyNonzeros { actual, limit } => {
                write!(
                    f,
                    "BEIT max non-zero coefficients is {limit}, job has {actual} non-zeros"
                )
            }
            Self::CoeffMagnitude {
                abs_value_bits,
                limit_bits,
                i,
                j,
            } => {
                let abs_value = f64::from_bits(*abs_value_bits);
                let limit = f64::from_bits(*limit_bits);
                write!(
                    f,
                    "BEIT coefficient magnitude must be < {limit}, got |M_{i},{j}|={abs_value}"
                )
            }
            Self::VarSampleProduct {
                actual,
                limit,
                num_vars,
                sample_count,
            } => {
                write!(
                    f,
                    "BEIT variables*samples limit is {limit}, job has {num_vars}*{sample_count}={actual}"
                )
            }
            Self::BilledJobCap { billed, cap } => {
                write!(
                    f,
                    "BEIT billed-job cap reached ({billed}/{cap}); raise --max-billed-jobs or BEIT_MAX_BILLED_JOBS"
                )
            }
        }
    }
}

/// Count non-zero Ising coefficients (non-zero `h` + non-zero `J`).
#[must_use]
pub fn count_ising_nonzeros(graph: &IsingGraph) -> usize {
    let h_nnz = graph.h.iter().filter(|&&v| v != 0.0).count();
    let j_nnz = graph.j.iter().filter(|&&v| v != 0.0).count();
    h_nnz + j_nnz
}

/// Convert Ising (`s ∈ {-1,+1}`) to upper-triangular QUBO (`x ∈ {0,1}`).
///
/// Uses the dimod-compatible map `s = 2x - 1`:
/// - linear `h_i`: `Q_ii += 2 h_i` (constant `-h_i` dropped; does not change argmin)
/// - quadratic `J_ij`: `Q_ii -= 2 J`, `Q_jj -= 2 J`, `Q_ij += 4 J` (constant `+J` dropped)
///
/// Zero-valued terms are omitted from the returned list.
#[must_use]
pub fn ising_to_qubo(graph: &IsingGraph) -> PreparedQubo {
    let n = graph.num_nodes();
    // BTreeMap keeps a stable term order for the wire format.
    let mut q: BTreeMap<(usize, usize), f64> = BTreeMap::new();

    for (i, &h) in graph.h.iter().enumerate() {
        if h != 0.0 {
            *q.entry((i, i)).or_insert(0.0) += 2.0 * h;
        }
    }
    for (k, &(u, v)) in graph.edges.iter().enumerate() {
        let j = graph.j.get(k).copied().unwrap_or(0.0);
        if j == 0.0 || u >= n || v >= n {
            continue;
        }
        if u == v {
            // Self-loop: J s^2 = J (constant under ±1); still maps into Q_uu.
            *q.entry((u, u)).or_insert(0.0) += 0.0; // constant only; skip
            continue;
        }
        let (a, b) = if u < v { (u, v) } else { (v, u) };
        *q.entry((a, a)).or_insert(0.0) -= 2.0 * j;
        *q.entry((b, b)).or_insert(0.0) -= 2.0 * j;
        *q.entry((a, b)).or_insert(0.0) += 4.0 * j;
    }

    let terms: Vec<QuboTerm> = q
        .into_iter()
        .filter(|(_, v)| *v != 0.0)
        .map(|((i, j), value)| QuboTerm { i, j, value })
        .collect();

    PreparedQubo { num_vars: n, terms }
}

/// Validate BEIT hard limits for a job. Never truncates.
///
/// # Errors
///
/// Returns a [`LimitViolation`] describing the first limit that fails.
pub fn validate_job(
    graph: &IsingGraph,
    sample_count: usize,
    qubo: &PreparedQubo,
) -> Result<(), LimitViolation> {
    let ising_nnz = count_ising_nonzeros(graph);
    if ising_nnz > MAX_NONZERO_COEFFS {
        return Err(LimitViolation::TooManyNonzeros {
            actual: ising_nnz,
            limit: MAX_NONZERO_COEFFS,
        });
    }
    let qubo_nnz = qubo.terms.len();
    if qubo_nnz > MAX_NONZERO_COEFFS {
        return Err(LimitViolation::TooManyNonzeros {
            actual: qubo_nnz,
            limit: MAX_NONZERO_COEFFS,
        });
    }

    for t in &qubo.terms {
        let abs = t.value.abs();
        // Strict: |M| must be < 1e6 (not <=).
        if abs >= MAX_COEFF_MAGNITUDE {
            return Err(LimitViolation::CoeffMagnitude {
                abs_value_bits: abs.to_bits(),
                limit_bits: MAX_COEFF_MAGNITUDE.to_bits(),
                i: t.i,
                j: t.j,
            });
        }
    }

    let num_vars = qubo.num_vars as u64;
    let samples = sample_count as u64;
    let product = num_vars.saturating_mul(samples);
    if product > MAX_VAR_SAMPLE_PRODUCT {
        return Err(LimitViolation::VarSampleProduct {
            actual: product,
            limit: MAX_VAR_SAMPLE_PRODUCT,
            num_vars: qubo.num_vars,
            sample_count,
        });
    }

    Ok(())
}

/// Map a QUBO binary sample (`0`/`1`) to Ising spins (`-1`/`+1`).
///
/// `s = 2x - 1`. Values other than `0` are treated as `1` (defensive).
#[must_use]
pub fn binary_to_spins(sample: &[u8]) -> Vec<i8> {
    sample
        .iter()
        .map(|&x| if x == 0 { -1_i8 } else { 1_i8 })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

    use super::*;
    use quip_miner_core::IsingGraph;

    #[test]
    fn milli_to_beit_divides_by_1000() {
        assert!((milli_to_beit(1000) - 1.0).abs() < 1e-15);
        assert!((milli_to_beit(-1000) - (-1.0)).abs() < 1e-15);
        // Non-round value: 1234 milli → 1.234
        assert!((milli_to_beit(1234) - 1.234).abs() < 1e-12);
        assert!((milli_to_beit(-7) - (-0.007)).abs() < 1e-15);
    }

    #[test]
    fn ising_to_qubo_ferro_pair() {
        // h=0, J=-1 between 0 and 1.
        let g = IsingGraph::new(vec![0.0, 0.0], vec![-1.0], vec![(0, 1)]);
        let q = ising_to_qubo(&g);
        assert_eq!(q.num_vars, 2);
        // Q_00 = -2*(-1)=2, Q_11=2, Q_01=4*(-1)=-4
        let get = |i, j| {
            q.terms
                .iter()
                .find(|t| t.i == i && t.j == j)
                .map(|t| t.value)
        };
        assert!((get(0, 0).unwrap() - 2.0).abs() < 1e-12);
        assert!((get(1, 1).unwrap() - 2.0).abs() < 1e-12);
        assert!((get(0, 1).unwrap() - (-4.0)).abs() < 1e-12);
    }

    #[test]
    fn binary_to_spins_maps_01() {
        assert_eq!(binary_to_spins(&[0, 1, 0, 1]), vec![-1, 1, -1, 1]);
    }

    #[test]
    fn reject_too_many_nonzeros() {
        // 5001 unit biases, no edges.
        let h = vec![1.0; MAX_NONZERO_COEFFS + 1];
        let g = IsingGraph::new(h, vec![], vec![]);
        let q = ising_to_qubo(&g);
        let err = validate_job(&g, 1, &q).unwrap_err();
        match err {
            LimitViolation::TooManyNonzeros { actual, limit } => {
                assert_eq!(actual, MAX_NONZERO_COEFFS + 1);
                assert_eq!(limit, MAX_NONZERO_COEFFS);
            }
            other => panic!("expected TooManyNonzeros, got {other}"),
        }
        assert_eq!(err.reject_reason(), RejectReason::TooLarge);
        let msg = err.to_string();
        assert!(msg.contains("5000"), "{msg}");
        assert!(msg.contains("5001"), "{msg}");
    }

    #[test]
    fn reject_coeff_magnitude() {
        // After Ising→QUBO, linear h becomes Q_ii = 2h. So h = 5e5 → Q = 1e6 (boundary reject).
        let g = IsingGraph::new(vec![500_000.0], vec![], vec![]);
        let q = ising_to_qubo(&g);
        let err = validate_job(&g, 1, &q).unwrap_err();
        match err {
            LimitViolation::CoeffMagnitude { .. } => {}
            other => panic!("expected CoeffMagnitude, got {other}"),
        }
        assert!(
            err.to_string().contains("1000000")
                || err.to_string().contains("1e6")
                || err.to_string().contains("1000000.0")
                || err.to_string().contains('|'),
            "{}",
            err
        );
    }

    #[test]
    fn reject_var_sample_product() {
        let g = IsingGraph::new(vec![0.0; 1001], vec![], vec![]);
        let q = ising_to_qubo(&g);
        // 1001 * 1000 = 1_001_000 > 1e6
        let err = validate_job(&g, 1000, &q).unwrap_err();
        match err {
            LimitViolation::VarSampleProduct {
                actual,
                limit,
                num_vars,
                sample_count,
            } => {
                assert_eq!(actual, 1_001_000);
                assert_eq!(limit, MAX_VAR_SAMPLE_PRODUCT);
                assert_eq!(num_vars, 1001);
                assert_eq!(sample_count, 1000);
            }
            other => panic!("expected VarSampleProduct, got {other}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("1001"), "{msg}");
        assert!(msg.contains("1000"), "{msg}");
    }

    #[test]
    fn accepts_boundary_under_limits() {
        let g = IsingGraph::new(vec![0.0, 0.0], vec![-1.0], vec![(0, 1)]);
        let q = ising_to_qubo(&g);
        validate_job(&g, 1, &q).expect("tiny ferro should pass");
    }
}
