# quip-solver-beit

BEIT cloud heuristic Ising miner for quip.network protocol version 2.

Binary: **`quip-beit-sa`**. Backend: BEIT `BEITUnconstrainedSampler` (quantum-inspired
simulated annealing over arbitrary topology). The exact Chimera-only `BEITSolver` is
**not** wrapped.

Energies are scored with the canonical `quip_protocol::scoring::energy_milli` (re-exported by
`quip-solver-core`) from
decoded spins. BEIT's own objective value is never forwarded as the protocol energy.

## Cost guard (read this first)

**Every sample job is a billed AWS Marketplace request** (heuristic pricing is on the
order of 30¢ per 100 seconds of sampler time; see BEIT docs).

| Control | Default | How to raise |
|---------|---------|--------------|
| Billed jobs per process | **10** | `--max-billed-jobs N` or `BEIT_MAX_BILLED_JOBS=N` |

When the cap is hit, further jobs are rejected with `Overloaded` and a log line that names
the cap. The process keeps running, so the cap holds until it exits. The cap counts per
process: a supervisor that restarts the solver starts a fresh one.
A throughput benchmark that leaves the default in place will stop after 10 paid calls —
raise the cap **explicitly** before measuring, and budget for the cost.

## Prerequisites (live path)

1. AWS Marketplace subscription for BEIT QUBO Solver and a customer key.
2. Python 3 with `beit-qubo-solver` installed:

   ```sh
   pip install beit-qubo-solver
   ```

3. Export the key (never commit it):

   ```sh
   export BEIT_CUSTOMER_KEY='your-key-here'
   ```

## Build

```sh
cargo build --release
```

The session loop and wire types come from the `quip-solver-core` crate on crates.io.

## Running

```sh
export BEIT_CUSTOMER_KEY=...
export QUIP_SESSION_TOKEN=...
quip-beit-sa --quip-coordinator unix:///run/quip/coord.sock
```

**Introspection:**

```sh
quip-beit-sa --capabilities   # JSON capabilities; no key required
quip-beit-sa --check          # key present + sidecar starts + ping (no billed sample)
```

`--check` fails with exit code 69 and an actionable message when `BEIT_CUSTOMER_KEY` is
missing or the sidecar cannot import the SDK. It does **not** call `sample_qubo`.

### Environment

| Variable | Purpose |
|----------|---------|
| `BEIT_CUSTOMER_KEY` | AWS Marketplace customer key (required for live/`--check`) |
| `BEIT_MAX_BILLED_JOBS` | Billed-job cap (default 10) |
| `BEIT_TIMEOUT_SECS` | Default sampler timeout seconds (default 10, max 720) |
| `BEIT_SIDECAR_PYTHON` | Python interpreter (default `python3`) |
| `BEIT_SIDECAR_SCRIPT` | Path to `beit_sidecar.py` |

Offline tests never need a key. They inject mock/replay transports in-process (same
idea as `ISINGMARK_ALLOW_MISSING_MINERS` in isingmark).

## BEIT limits (enforced; jobs are rejected, never truncated)

| Limit | Value |
|-------|-------|
| Non-zero coefficients | ≤ 5000 |
| Coefficient magnitude | \|M_ij\| < 10^6 |
| variables × samples | ≤ 10^6 |
| Topology | arbitrary |

Coefficient mapping: wire milli-integers become BEIT floats via `milli / 1000`. The
miner converts Ising → QUBO before submit (dimod-compatible).

## Sidecar

`python/beit_sidecar.py` is spawned **once** at startup. The Rust miner and the sidecar
exchange newline-delimited JSON on stdin/stdout. If the sidecar dies mid-job, the job
fails with a clear error (no hang).

## Tests

```sh
cargo test --all-targets
# no BEIT_CUSTOMER_KEY, no network
```

```sh
ruff check python/
ruff format --check python/
```

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
