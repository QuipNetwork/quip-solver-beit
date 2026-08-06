//! Reject paths, coefficient scaling, rescoring, and transport protocol tests.
//!
//! All tests run offline (no `BEIT_CUSTOMER_KEY`, no network) via mock/replay
//! transports — same spirit as `ISINGMARK_ALLOW_MISSING_MINERS`.

use quip_miner_beit::limits::{
    binary_to_spins, count_ising_nonzeros, ising_to_qubo, milli_to_beit, validate_job,
    LimitViolation, MAX_NONZERO_COEFFS, MAX_VAR_SAMPLE_PRODUCT,
};
use quip_miner_beit::transport::{
    decode_response, encode_request, DeadTransport, MockTransport, ReplayTransport, SampleResponse,
    SidecarRequest, SidecarSample,
};
use quip_miner_beit::{BeitConfig, BeitSampler};
use quip_miner_core::{IsingGraph, SampleParams, Sampler};
use quip_proto::v1::RejectReason;
use quip_protocol::scoring::energy_milli;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn params(reads: usize) -> SampleParams {
    SampleParams {
        num_reads: reads,
        num_sweeps: 1,
        seed: 1,
        ..Default::default()
    }
}

#[test]
fn milli_to_beit_non_round_value() {
    assert!((milli_to_beit(1000) - 1.0).abs() < 1e-15);
    assert!((milli_to_beit(1234) - 1.234).abs() < 1e-12);
    assert!((milli_to_beit(-250) - (-0.25)).abs() < 1e-15);
}

#[test]
fn reject_over_5000_nonzeros_names_limit() {
    let h: Vec<f64> = (0..=MAX_NONZERO_COEFFS)
        .map(|i| if i < MAX_NONZERO_COEFFS + 1 { 1.0 } else { 0.0 })
        .collect();
    assert_eq!(h.len(), MAX_NONZERO_COEFFS + 1);
    let g = IsingGraph::new(h, vec![], vec![]);
    assert_eq!(count_ising_nonzeros(&g), MAX_NONZERO_COEFFS + 1);
    let q = ising_to_qubo(&g);
    let err = validate_job(&g, 1, &q).expect_err("must reject");
    let msg = err.to_string();
    assert!(msg.contains("5000"), "{msg}");
    assert!(msg.contains(&(MAX_NONZERO_COEFFS + 1).to_string()), "{msg}");
    assert_eq!(err.reject_reason(), RejectReason::TooLarge);

    let sampler = BeitSampler::new(
        Arc::new(MockTransport {
            samples: vec![vec![0; MAX_NONZERO_COEFFS + 1]],
        }),
        BeitConfig::default(),
    );
    assert_eq!(
        sampler.sample(&g, &params(1)).unwrap_err(),
        RejectReason::TooLarge
    );
    assert_eq!(sampler.billed_count(), 0);
}

#[test]
fn reject_over_magnitude_coefficient() {
    // h = 5e5 → Q_ii = 1e6 which is NOT strictly < 1e6.
    let g = IsingGraph::new(vec![500_000.0], vec![], vec![]);
    let q = ising_to_qubo(&g);
    let err = validate_job(&g, 1, &q).expect_err("magnitude");
    assert!(matches!(err, LimitViolation::CoeffMagnitude { .. }));
    let msg = err.to_string();
    assert!(
        msg.contains("1000000") || msg.contains("1e6") || msg.contains('|'),
        "{msg}"
    );

    let sampler = BeitSampler::new(
        Arc::new(MockTransport {
            samples: vec![vec![0]],
        }),
        BeitConfig::default(),
    );
    assert_eq!(
        sampler.sample(&g, &params(1)).unwrap_err(),
        RejectReason::TooLarge
    );
}

#[test]
fn reject_var_sample_product() {
    let n = 1001;
    let g = IsingGraph::new(vec![0.0; n], vec![], vec![]);
    let q = ising_to_qubo(&g);
    let err = validate_job(&g, 1000, &q).expect_err("product");
    let LimitViolation::VarSampleProduct {
        actual,
        limit,
        num_vars,
        sample_count,
    } = err
    else {
        // Match cpu-miner test style: avoid panic! under clippy::panic deny.
        assert_eq!("got", "VarSampleProduct");
        return;
    };
    assert_eq!(actual, 1_001_000);
    assert_eq!(limit, MAX_VAR_SAMPLE_PRODUCT);
    assert_eq!(num_vars, n);
    assert_eq!(sample_count, 1000);
    let msg = err.to_string();
    assert!(msg.contains("1001"), "{msg}");
    assert!(
        msg.contains("1000000")
            || msg.contains("10^6")
            || msg.contains("1e6")
            || msg.contains(&MAX_VAR_SAMPLE_PRODUCT.to_string()),
        "{msg}"
    );
}

#[test]
fn local_rescoring_ignores_beit_energy() {
    let g = IsingGraph::new(vec![0.0, 0.0], vec![-1.0], vec![(0, 1)]);
    // bits [1,1] → spins [+1,+1], energy = -1 → -1000 milli
    let transport = ReplayTransport::new(vec![Ok(SampleResponse {
        samples: vec![SidecarSample {
            bits: vec![1, 1],
            energy: Some(42.0), // garbage — must be ignored
        }],
    })]);
    let sampler = BeitSampler::new(Arc::new(transport), BeitConfig::default());
    let results = sampler.sample(&g, &params(1)).expect("ok");
    let expected = energy_milli(&results[0].spins, &g.h, &g.j, &g.edges);
    assert_eq!(results[0].energy_milli, expected);
    assert_eq!(results[0].energy_milli, -1000);
    assert_eq!(binary_to_spins(&[1, 1]), vec![1, 1]);
}

#[test]
fn sidecar_protocol_encode_decode() {
    let req = SidecarRequest::Sample {
        sample_count: 2,
        timeout: 5,
        qubo: vec![(0, 0, 1.234), (0, 1, -0.5)],
    };
    let line = encode_request(&req).expect("encode");
    let parsed: SidecarRequest = serde_json::from_str(&line).expect("parse");
    assert_eq!(parsed, req);

    let resp =
        decode_response(r#"{"ok":true,"samples":[[0,1]],"energies":[-0.5]}"#).expect("decode");
    assert!(resp.ok);
    assert_eq!(resp.samples.as_ref().unwrap()[0], vec![0, 1]);
}

#[test]
fn sidecar_death_mid_job_errors_quickly() {
    let g = IsingGraph::new(vec![0.0, 0.0], vec![-1.0], vec![(0, 1)]);
    let sampler = BeitSampler::new(Arc::new(DeadTransport), BeitConfig::default());
    let t0 = Instant::now();
    let err = sampler.sample(&g, &params(1)).unwrap_err();
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "sidecar death must not hang"
    );
    assert_eq!(err, RejectReason::Overloaded);
}

#[test]
fn never_truncates_to_fit_nonzeros() {
    // Building a 5001-nnz problem must reject, not silently drop terms.
    let h = vec![0.5; MAX_NONZERO_COEFFS + 1];
    let g = IsingGraph::new(h, vec![], vec![]);
    let q = ising_to_qubo(&g);
    assert!(q.terms.len() > MAX_NONZERO_COEFFS);
    assert!(validate_job(&g, 1, &q).is_err());
}
