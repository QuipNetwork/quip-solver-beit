//! Sidecar transport: long-lived Python child or in-process mock/replay.
//!
//! The live path spawns **one** `python/beit_sidecar.py` process and exchanges
//! newline-delimited JSON over its stdin/stdout. Import cost stays off the
//! per-job path. Tests inject [`ReplayTransport`] so the suite needs no key
//! and no network (same spirit as `ISINGMARK_ALLOW_MISSING_MINERS`).

use crate::limits::PreparedQubo;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Environment variable holding the AWS Marketplace customer key.
pub const CUSTOMER_KEY_ENV: &str = "BEIT_CUSTOMER_KEY";

/// Optional override for the Python interpreter used to run the sidecar.
pub const SIDECAR_PYTHON_ENV: &str = "BEIT_SIDECAR_PYTHON";

/// Optional override for the sidecar script path.
pub const SIDECAR_SCRIPT_ENV: &str = "BEIT_SIDECAR_SCRIPT";

/// Transport-level error (sidecar death, protocol, mock exhaustion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransportError {}

/// One sample returned by the sidecar: binary `0`/`1` bits, optional energy.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SidecarSample {
    /// Binary assignment, length = number of variables.
    pub bits: Vec<u8>,
    /// BEIT-reported objective if present; **never** used as protocol energy.
    #[serde(default)]
    pub energy: Option<f64>,
}

/// Successful sample response from the sidecar.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SampleResponse {
    /// Returned samples (may be fewer than requested).
    pub samples: Vec<SidecarSample>,
}

/// Abstraction over the Python BEIT sidecar.
pub trait BeitTransport: Send + Sync {
    /// No-cost health check (import + connection construct). Must not bill.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError`] when the backend is not usable.
    fn ping(&self) -> Result<(), TransportError>;

    /// Run one heuristic sample job (billed on the live path).
    ///
    /// # Errors
    ///
    /// Returns [`TransportError`] on protocol failure, sidecar death, or
    /// mock/replay exhaustion.
    fn sample(
        &self,
        qubo: &PreparedQubo,
        sample_count: usize,
        timeout_secs: u64,
    ) -> Result<SampleResponse, TransportError>;
}

// --- Wire types (shared by live sidecar and encode/decode tests) ---

/// Outbound request line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SidecarRequest {
    /// No-cost reachability / import check.
    Ping,
    /// Billed sample.
    Sample {
        /// Number of samples to request.
        sample_count: usize,
        /// Timeout in seconds.
        timeout: u64,
        /// QUBO terms as `[i, j, value]`.
        qubo: Vec<(usize, usize, f64)>,
    },
}

/// Inbound response line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SidecarResponse {
    /// Whether the command succeeded.
    pub ok: bool,
    /// Error message when `ok` is false.
    #[serde(default)]
    pub error: Option<String>,
    /// Echoed command name (optional).
    #[serde(default)]
    pub cmd: Option<String>,
    /// Binary samples as arrays of 0/1.
    #[serde(default)]
    pub samples: Option<Vec<Vec<u8>>>,
    /// Optional BEIT energies aligned with `samples` (ignored for scoring).
    #[serde(default)]
    pub energies: Option<Vec<f64>>,
}

/// Encode a request to a single JSON line (no trailing newline).
///
/// # Errors
///
/// Returns [`TransportError`] if serialization fails.
pub fn encode_request(req: &SidecarRequest) -> Result<String, TransportError> {
    serde_json::to_string(req).map_err(|e| TransportError(format!("encode request: {e}")))
}

/// Decode a response line.
///
/// # Errors
///
/// Returns [`TransportError`] if the line is not valid JSON of the expected shape.
pub fn decode_response(line: &str) -> Result<SidecarResponse, TransportError> {
    serde_json::from_str(line.trim())
        .map_err(|e| TransportError(format!("decode response: {e}: {line}")))
}

fn qubo_to_wire(qubo: &PreparedQubo) -> Vec<(usize, usize, f64)> {
    qubo.terms.iter().map(|t| (t.i, t.j, t.value)).collect()
}

fn response_to_samples(resp: SidecarResponse) -> Result<SampleResponse, TransportError> {
    if !resp.ok {
        return Err(TransportError(resp.error.unwrap_or_else(|| {
            "sidecar returned ok=false with no error".to_owned()
        })));
    }
    let raw = resp.samples.unwrap_or_default();
    let energies = resp.energies.unwrap_or_default();
    let mut samples = Vec::with_capacity(raw.len());
    for (idx, bits) in raw.into_iter().enumerate() {
        let energy = energies.get(idx).copied();
        samples.push(SidecarSample { bits, energy });
    }
    Ok(SampleResponse { samples })
}

// --- Live Python sidecar ---

struct SidecarPipes {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Long-lived Python `beit_sidecar.py` process.
pub struct PythonSidecar {
    inner: Mutex<SidecarPipes>,
    /// Monotonic billed sample calls made through this sidecar.
    billed: AtomicU64,
}

impl PythonSidecar {
    /// Spawn the sidecar. Does not require a billed solve.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError`] if the key is missing, the script cannot be
    /// found, or the process fails to start / answer the initial ping.
    pub fn spawn() -> Result<Self, TransportError> {
        let key = std::env::var(CUSTOMER_KEY_ENV).map_err(|_| {
            TransportError(format!(
                "{CUSTOMER_KEY_ENV} is not set; export your AWS Marketplace customer key \
                 (never commit it). Example: export {CUSTOMER_KEY_ENV}=<key>"
            ))
        })?;
        if key.trim().is_empty() {
            return Err(TransportError(format!(
                "{CUSTOMER_KEY_ENV} is empty; set a non-empty AWS Marketplace customer key"
            )));
        }

        let python = std::env::var(SIDECAR_PYTHON_ENV).unwrap_or_else(|_| "python3".to_owned());
        let script = resolve_sidecar_script()?;

        let mut child = Command::new(&python)
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env(CUSTOMER_KEY_ENV, key)
            .spawn()
            .map_err(|e| {
                TransportError(format!(
                    "failed to spawn BEIT sidecar ({python} {}): {e}",
                    script.display()
                ))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError("sidecar stdin missing".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError("sidecar stdout missing".to_owned()))?;

        let sidecar = Self {
            inner: Mutex::new(SidecarPipes {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            billed: AtomicU64::new(0),
        };

        // Fail fast if the process dies on import before any job.
        sidecar.ping()?;
        Ok(sidecar)
    }

    fn transact_locked(
        pipes: &mut SidecarPipes,
        req: &SidecarRequest,
    ) -> Result<SidecarResponse, TransportError> {
        // Detect death before write so we do not hang on a broken pipe forever.
        if let Ok(Some(status)) = pipes.child.try_wait() {
            return Err(TransportError(format!(
                "BEIT sidecar exited before request (status {status})"
            )));
        }

        let line = encode_request(req)?;
        writeln!(pipes.stdin, "{line}").map_err(|e| {
            TransportError(format!(
                "BEIT sidecar write failed (process likely dead): {e}"
            ))
        })?;
        pipes
            .stdin
            .flush()
            .map_err(|e| TransportError(format!("BEIT sidecar flush failed: {e}")))?;

        let mut resp_line = String::new();
        let n = pipes.stdout.read_line(&mut resp_line).map_err(|e| {
            TransportError(format!(
                "BEIT sidecar read failed (process likely dead): {e}"
            ))
        })?;
        if n == 0 {
            // EOF without a line: child closed stdout.
            let status = pipes.child.try_wait().ok().flatten();
            return Err(TransportError(format!(
                "BEIT sidecar closed stdout (EOF); exit status={status:?}"
            )));
        }
        decode_response(&resp_line)
    }

    /// Number of billed `sample` calls issued.
    #[must_use]
    pub fn billed_count(&self) -> u64 {
        self.billed.load(Ordering::Relaxed)
    }
}

impl BeitTransport for PythonSidecar {
    fn ping(&self) -> Result<(), TransportError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| TransportError("sidecar mutex poisoned".to_owned()))?;
        let resp = Self::transact_locked(&mut guard, &SidecarRequest::Ping)?;
        if !resp.ok {
            return Err(TransportError(
                resp.error.unwrap_or_else(|| "ping failed".to_owned()),
            ));
        }
        Ok(())
    }

    fn sample(
        &self,
        qubo: &PreparedQubo,
        sample_count: usize,
        timeout_secs: u64,
    ) -> Result<SampleResponse, TransportError> {
        let req = SidecarRequest::Sample {
            sample_count,
            timeout: timeout_secs,
            qubo: qubo_to_wire(qubo),
        };
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| TransportError("sidecar mutex poisoned".to_owned()))?;
        let resp = Self::transact_locked(&mut guard, &req)?;
        self.billed.fetch_add(1, Ordering::Relaxed);
        response_to_samples(resp)
    }
}

impl Drop for PythonSidecar {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            let _ = guard.child.kill();
            let _ = guard.child.wait();
        }
    }
}

fn resolve_sidecar_script() -> Result<PathBuf, TransportError> {
    if let Ok(p) = std::env::var(SIDECAR_SCRIPT_ENV) {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        return Err(TransportError(format!(
            "{SIDECAR_SCRIPT_ENV}={path:?} is not a file"
        )));
    }
    // Walk from CARGO_MANIFEST_DIR (tests/bin) and from cwd.
    let candidates = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("python/beit_sidecar.py"),
        PathBuf::from("python/beit_sidecar.py"),
        PathBuf::from("beit_sidecar.py"),
    ];
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    Err(TransportError(
        "cannot find python/beit_sidecar.py; set BEIT_SIDECAR_SCRIPT".to_owned(),
    ))
}

// --- Offline mock / replay ---

/// Replay pre-recorded responses in order (unit tests, no network).
pub struct ReplayTransport {
    responses: Mutex<Vec<Result<SampleResponse, TransportError>>>,
    ping_ok: bool,
}

impl ReplayTransport {
    /// Build a transport that returns `responses` on successive `sample` calls.
    #[must_use]
    pub fn new(responses: Vec<Result<SampleResponse, TransportError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
            ping_ok: true,
        }
    }

    /// Build a transport whose `ping` fails (no sample responses).
    #[must_use]
    pub fn ping_fails() -> Self {
        Self {
            responses: Mutex::new(Vec::new()),
            ping_ok: false,
        }
    }
}

impl BeitTransport for ReplayTransport {
    fn ping(&self) -> Result<(), TransportError> {
        if self.ping_ok {
            Ok(())
        } else {
            Err(TransportError(
                "replay transport: ping configured to fail".to_owned(),
            ))
        }
    }

    fn sample(
        &self,
        _qubo: &PreparedQubo,
        _sample_count: usize,
        _timeout_secs: u64,
    ) -> Result<SampleResponse, TransportError> {
        let mut guard = self
            .responses
            .lock()
            .map_err(|_| TransportError("replay mutex poisoned".to_owned()))?;
        if guard.is_empty() {
            return Err(TransportError(
                "replay transport exhausted (no more recorded responses)".to_owned(),
            ));
        }
        guard.remove(0)
    }
}

/// Transport that always returns a fixed set of binary samples (ignore request).
pub struct MockTransport {
    /// Fixed samples to return on every call.
    pub samples: Vec<Vec<u8>>,
}

impl BeitTransport for MockTransport {
    fn ping(&self) -> Result<(), TransportError> {
        Ok(())
    }

    fn sample(
        &self,
        _qubo: &PreparedQubo,
        sample_count: usize,
        _timeout_secs: u64,
    ) -> Result<SampleResponse, TransportError> {
        let mut out = Vec::new();
        for i in 0..sample_count {
            let bits = self
                .samples
                .get(i % self.samples.len().max(1))
                .cloned()
                .unwrap_or_default();
            out.push(SidecarSample { bits, energy: None });
        }
        Ok(SampleResponse { samples: out })
    }
}

/// Dead-sidecar simulator: every call returns a death error immediately.
pub struct DeadTransport;

impl BeitTransport for DeadTransport {
    fn ping(&self) -> Result<(), TransportError> {
        Err(TransportError(
            "BEIT sidecar closed stdout (EOF); exit status=Some(ExitStatus(signal: 9))".to_owned(),
        ))
    }

    fn sample(
        &self,
        _qubo: &PreparedQubo,
        _sample_count: usize,
        _timeout_secs: u64,
    ) -> Result<SampleResponse, TransportError> {
        // Return promptly — must not hang.
        let _ = Duration::from_millis(0);
        Err(TransportError(
            "BEIT sidecar write failed (process likely dead): Broken pipe".to_owned(),
        ))
    }
}

/// Whether a path looks like the shipped sidecar (test helper).
#[must_use]
pub fn default_sidecar_script_exists() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("python/beit_sidecar.py")
        .is_file()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

    use super::*;
    use crate::limits::{ising_to_qubo, PreparedQubo, QuboTerm};
    use quip_miner_core::IsingGraph;

    #[test]
    fn encode_decode_ping_roundtrip() {
        let line = encode_request(&SidecarRequest::Ping).unwrap();
        assert!(line.contains("ping"), "{line}");
        // Response shape
        let resp = decode_response(r#"{"ok":true,"cmd":"ping"}"#).unwrap();
        assert!(resp.ok);
    }

    #[test]
    fn encode_decode_sample_roundtrip() {
        let req = SidecarRequest::Sample {
            sample_count: 2,
            timeout: 10,
            qubo: vec![(0, 0, 1.0), (0, 1, -4.0), (1, 1, 1.0)],
        };
        let line = encode_request(&req).unwrap();
        let back: SidecarRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(back, req);

        let resp =
            decode_response(r#"{"ok":true,"samples":[[0,1],[1,0]],"energies":[-1.0, -1.0]}"#)
                .unwrap();
        let samples = response_to_samples(resp).unwrap();
        assert_eq!(samples.samples.len(), 2);
        assert_eq!(samples.samples[0].bits, vec![0, 1]);
        assert_eq!(samples.samples[0].energy, Some(-1.0));
    }

    #[test]
    fn replay_transport_returns_recorded() {
        let g = IsingGraph::new(vec![0.0, 0.0], vec![-1.0], vec![(0, 1)]);
        let q = ising_to_qubo(&g);
        let t = ReplayTransport::new(vec![Ok(SampleResponse {
            samples: vec![SidecarSample {
                bits: vec![0, 1],
                energy: Some(99.0),
            }],
        })]);
        t.ping().unwrap();
        let r = t.sample(&q, 1, 1).unwrap();
        assert_eq!(r.samples[0].bits, vec![0, 1]);
        // Second call exhausts
        assert!(t.sample(&q, 1, 1).is_err());
    }

    #[test]
    fn dead_transport_errors_immediately() {
        let qubo = PreparedQubo {
            num_vars: 1,
            terms: vec![QuboTerm {
                i: 0,
                j: 0,
                value: 1.0,
            }],
        };
        let t0 = std::time::Instant::now();
        let err = DeadTransport.sample(&qubo, 1, 1).unwrap_err();
        assert!(t0.elapsed() < Duration::from_secs(2), "must not hang");
        assert!(
            err.0.contains("dead") || err.0.contains("Broken") || err.0.contains("EOF"),
            "{err}"
        );
    }

    #[test]
    fn mock_transport_repeats_samples() {
        let t = MockTransport {
            samples: vec![vec![1, 0]],
        };
        let qubo = PreparedQubo {
            num_vars: 2,
            terms: vec![],
        };
        let r = t.sample(&qubo, 3, 1).unwrap();
        assert_eq!(r.samples.len(), 3);
        assert_eq!(r.samples[2].bits, vec![1, 0]);
    }
}
