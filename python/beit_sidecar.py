#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Long-lived BEITUnconstrainedSampler sidecar for quip-beit-sa.

Reads newline-delimited JSON requests from stdin and writes one JSON response
line per request to stdout. The customer key is taken from BEIT_CUSTOMER_KEY
and is never logged.

Wire protocol
-------------
Request:
  {"cmd":"ping"}
  {"cmd":"sample","sample_count":2,"timeout":10,"qubo":[[0,0,1.0],[0,1,-4.0]]}

Response:
  {"ok":true,"cmd":"ping"}
  {"ok":true,"samples":[[0,1],[1,0]],"energies":[-1.0,-1.0]}
  {"ok":false,"error":"..."}

`ping` constructs AWSSolverConnection only (no sample_qubo, no billed call).
"""

from __future__ import annotations

import json
import os
import sys
import traceback
from typing import Any

CUSTOMER_KEY_ENV = "BEIT_CUSTOMER_KEY"


def _fail(msg: str, **extra: Any) -> None:
    payload = {"ok": False, "error": msg}
    payload.update(extra)
    sys.stdout.write(json.dumps(payload, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def _ok(payload: dict[str, Any]) -> None:
    body = {"ok": True}
    body.update(payload)
    sys.stdout.write(json.dumps(body, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def _load_sampler():
    """Import SDK and build BEITUnconstrainedSampler. Raises on failure."""
    key = os.environ.get(CUSTOMER_KEY_ENV, "").strip()
    if not key:
        raise RuntimeError(
            f"{CUSTOMER_KEY_ENV} is not set or empty; export your AWS Marketplace customer key"
        )
    # Lazy import so a missing package yields a clear error line.
    from beit.qubo_solver.beit_solver import BEITUnconstrainedSampler
    from beit.qubo_solver.solver_connection import AWSSolverConnection

    connection = AWSSolverConnection(key)
    return BEITUnconstrainedSampler(connection)


def _qubo_dict(terms: list[Any]) -> dict[tuple[int, int], float]:
    out: dict[tuple[int, int], float] = {}
    for item in terms:
        if not isinstance(item, (list, tuple)) or len(item) != 3:
            raise ValueError(f"qubo term must be [i,j,value], got {item!r}")
        i, j, val = int(item[0]), int(item[1]), float(item[2])
        if i > j:
            i, j = j, i
        out[(i, j)] = out.get((i, j), 0.0) + val
    return out


def main() -> int:
    # Line-buffer stdout so each response is visible immediately.
    reconfigure = getattr(sys.stdout, "reconfigure", None)
    if callable(reconfigure):
        reconfigure(line_buffering=True)

    sampler = None
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as exc:
            _fail(f"invalid json: {exc}")
            continue

        cmd = req.get("cmd")
        try:
            if cmd == "ping":
                if sampler is None:
                    sampler = _load_sampler()
                _ok({"cmd": "ping"})
                continue

            if cmd == "sample":
                if sampler is None:
                    sampler = _load_sampler()
                sample_count = int(req.get("sample_count", 1))
                timeout = int(req.get("timeout", 10))
                qubo_terms = req.get("qubo") or []
                qubo = _qubo_dict(qubo_terms)
                result = sampler.sample_qubo(
                    qubo,
                    sample_count=sample_count,
                    timeout=timeout,
                )
                # dimod SampleSet: iterate records
                samples: list[list[int]] = []
                energies: list[float] = []
                variables = list(result.variables)
                for sample, energy in result.data(["sample", "energy"]):
                    bits = [int(sample[v]) for v in variables]
                    samples.append(bits)
                    energies.append(float(energy))
                _ok({"samples": samples, "energies": energies})
                continue

            _fail(f"unknown cmd: {cmd!r}")
        except Exception as exc:  # noqa: BLE001 — surface any SDK failure to Rust
            # Never include the customer key in the error text.
            msg = f"{type(exc).__name__}: {exc}"
            # Keep traceback on stderr for operators; stdout stays protocol-only.
            traceback.print_exc(file=sys.stderr)
            _fail(msg)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
