#!/usr/bin/env python3
"""Local benchmark for DeltaGlider S3 adapter paths.

Compares the legacy Axum S3 adapter with the `s3s` adapter using the same
compiled binary and a filesystem backend. The benchmark intentionally uses raw
HTTP and open auth so it measures request parsing/routing/engine behavior, not
AWS SDK client overhead or SigV4 signing cost.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "release" / "deltaglider_proxy"
BUCKET = "bench-bucket"


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, int(round((len(ordered) - 1) * pct)))
    return ordered[idx]


class Client:
    def __init__(self, port: int) -> None:
        self.port = port
        self.conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)

    def request(
        self,
        method: str,
        path: str,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
        expected: tuple[int, ...] = (200,),
    ) -> tuple[int, dict[str, str], bytes]:
        headers = headers or {}
        if body is not None and "Content-Length" not in headers:
            headers["Content-Length"] = str(len(body))
        try:
            self.conn.request(method, path, body=body, headers=headers)
            resp = self.conn.getresponse()
            data = resp.read()
        except (http.client.HTTPException, OSError):
            self.conn.close()
            self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)
            self.conn.request(method, path, body=body, headers=headers)
            resp = self.conn.getresponse()
            data = resp.read()
        status = resp.status
        header_map = {k.lower(): v for k, v in resp.getheaders()}
        if status not in expected:
            raise RuntimeError(f"{method} {path} returned {status}: {data[:200]!r}")
        return status, header_map, data

    def close(self) -> None:
        self.conn.close()


def wait_ready(port: int, proc: subprocess.Popen[bytes]) -> None:
    deadline = time.time() + 20
    while time.time() < deadline:
        if proc.poll() is not None:
            raise RuntimeError(f"proxy exited early with {proc.returncode}")
        try:
            c = Client(port)
            c.request("GET", "/_/health")
            c.close()
            return
        except Exception:
            time.sleep(0.1)
    raise TimeoutError(f"proxy on port {port} did not become ready")


def start_server(adapter: str, port: int, tmp: Path) -> subprocess.Popen[bytes]:
    data_dir = tmp / f"{adapter}-data"
    data_dir.mkdir()
    config = tmp / f"{adapter}.toml"
    config.write_text(
        "\n".join(
            [
                f'listen_addr = "127.0.0.1:{port}"',
                'authentication = "none"',
                "max_object_size = 134217728",
                "",
                "[backend]",
                'type = "filesystem"',
                f'path = "{data_dir}"',
                "",
            ]
        )
    )
    env = os.environ.copy()
    env.update(
        {
            "DGP_CONFIG": str(config),
            "DGP_S3_ADAPTER": adapter,
            "DGP_DEBUG_HEADERS": "true",
            "RUST_LOG": "deltaglider_proxy=error",
        }
    )
    proc = subprocess.Popen(
        [str(BIN)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        env=env,
        cwd=str(ROOT),
    )
    wait_ready(port, proc)
    client = Client(port)
    client.request("PUT", f"/{BUCKET}", expected=(200, 409))
    client.close()
    return proc


def stop_server(proc: subprocess.Popen[bytes]) -> None:
    if proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def run_timed(count: int, op: Callable[[int], int]) -> tuple[list[float], int]:
    durations: list[float] = []
    total_bytes = 0
    for i in range(count):
        start = time.perf_counter()
        total_bytes += op(i)
        durations.append(time.perf_counter() - start)
    return durations, total_bytes


def summarize(name: str, durations: list[float], total_bytes: int) -> dict[str, float | str]:
    total = sum(durations)
    return {
        "case": name,
        "ops": len(durations),
        "total_s": round(total, 4),
        "ops_s": round(len(durations) / total, 2) if total else 0.0,
        "median_ms": round(statistics.median(durations) * 1000, 3),
        "p95_ms": round(percentile(durations, 0.95) * 1000, 3),
        "mb_s": round((total_bytes / (1024 * 1024)) / total, 2) if total and total_bytes else 0.0,
    }


def benchmark_adapter(adapter: str, port: int, args: argparse.Namespace) -> list[dict[str, float | str]]:
    small_body = b"x" * args.small_size
    large_body = bytes((i * 31) & 0xFF for i in range(args.large_size))
    range_header = {"Range": f"bytes=0-{args.range_size - 1}"}
    content_type = {"Content-Type": "application/octet-stream"}

    with tempfile.TemporaryDirectory(prefix=f"dgp-{adapter}-bench-") as d:
        proc = start_server(adapter, port, Path(d))
        client = Client(port)
        try:
            # Warmup.
            for i in range(args.warmup):
                client.request(
                    "PUT",
                    f"/{BUCKET}/warmup-{i}.bin",
                    body=small_body,
                    headers=content_type.copy(),
                )
                client.request("GET", f"/{BUCKET}/warmup-{i}.bin")

            rows = []

            durations, written = run_timed(
                args.small_ops,
                lambda i: (
                    client.request(
                        "PUT",
                        f"/{BUCKET}/small-{i}.bin",
                        body=small_body,
                        headers=content_type.copy(),
                    )
                    and len(small_body)
                ),
            )
            rows.append(summarize("small_put", durations, written))

            durations, read = run_timed(
                args.small_ops,
                lambda i: len(client.request("GET", f"/{BUCKET}/small-{i}.bin")[2]),
            )
            rows.append(summarize("small_get", durations, read))

            durations, written = run_timed(
                args.large_ops,
                lambda i: (
                    client.request(
                        "PUT",
                        f"/{BUCKET}/large-{i}.bin",
                        body=large_body,
                        headers=content_type.copy(),
                    )
                    and len(large_body)
                ),
            )
            rows.append(summarize("large_put", durations, written))

            durations, read = run_timed(
                args.large_ops,
                lambda i: len(client.request("GET", f"/{BUCKET}/large-{i}.bin")[2]),
            )
            rows.append(summarize("large_get", durations, read))

            durations, read = run_timed(
                args.range_ops,
                lambda i: len(
                    client.request(
                        "GET",
                        f"/{BUCKET}/large-{i % args.large_ops}.bin",
                        headers=range_header.copy(),
                        expected=(206,),
                    )[2]
                ),
            )
            rows.append(summarize("range_get", durations, read))

            durations, read = run_timed(
                args.list_ops,
                lambda _i: len(
                    client.request(
                        "GET",
                        f"/{BUCKET}?list-type=2&prefix=small-&max-keys=1000",
                    )[2]
                ),
            )
            rows.append(summarize("list_v2", durations, read))

            return rows
        finally:
            client.close()
            stop_server(proc)


def print_table(results: dict[str, list[dict[str, float | str]]]) -> None:
    headers = ["adapter", "case", "ops", "total_s", "ops_s", "median_ms", "p95_ms", "mb_s"]
    rows: list[dict[str, float | str]] = []
    for adapter, entries in results.items():
        for entry in entries:
            row = {"adapter": adapter, **entry}
            rows.append(row)
    widths = {
        header: max(len(header), *(len(str(row[header])) for row in rows))
        for header in headers
    }
    print("  ".join(header.ljust(widths[header]) for header in headers))
    print("  ".join("-" * widths[header] for header in headers))
    for row in rows:
        print("  ".join(str(row[header]).ljust(widths[header]) for header in headers))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--small-ops", type=int, default=300)
    parser.add_argument("--small-size", type=int, default=4 * 1024)
    parser.add_argument("--large-ops", type=int, default=5)
    parser.add_argument("--large-size", type=int, default=16 * 1024 * 1024)
    parser.add_argument("--range-ops", type=int, default=500)
    parser.add_argument("--range-size", type=int, default=1024 * 1024)
    parser.add_argument("--list-ops", type=int, default=100)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--base-port", type=int, default=19700)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()

    if not BIN.exists():
        print(f"missing binary: {BIN}; run cargo build --release --features s3s-adapter", file=sys.stderr)
        return 2

    results = {
        "axum": benchmark_adapter("axum", args.base_port, args),
        "s3s": benchmark_adapter("s3s", args.base_port + 1, args),
    }

    print_table(results)
    if args.json:
        args.json.write_text(json.dumps(results, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
