from __future__ import annotations

import concurrent.futures
import hashlib
import json
import time
from dataclasses import asdict
from pathlib import Path
from typing import Any, Iterable

from .artifacts import prepare_artifacts
from .config import parse_mode_buckets, slug_now
from .metrics import maybe_restart_proxy, snapshot_proxy
from .model import Artifact, Endpoint, OpResult
from .reporting import summarize_ops, write_markdown_report, write_rows, write_summary
from .sigv4 import SigV4Client
from .util import capture_local_environment


def put_object(
    client: SigV4Client,
    bucket: str,
    key: str,
    artifact: Artifact,
    mode: str,
    concurrency: int,
    run_id: str,
) -> OpResult:
    start = time.perf_counter()
    try:
        body = Path(artifact.path).read_bytes()
        status, _, _ = client.request(
            "PUT",
            bucket,
            key,
            body=body,
            extra_headers={"Content-Type": "application/octet-stream"},
        )
        return OpResult(run_id, mode, "put", concurrency, "PUT", bucket, key, artifact.bytes, time.perf_counter() - start, status, True)
    except Exception as e:
        return OpResult(run_id, mode, "put", concurrency, "PUT", bucket, key, artifact.bytes, time.perf_counter() - start, 0, False, str(e))


def get_object(
    client: SigV4Client,
    bucket: str,
    key: str,
    artifact: Artifact,
    mode: str,
    phase: str,
    concurrency: int,
    run_id: str,
) -> OpResult:
    start = time.perf_counter()
    try:
        status, _, data = client.request("GET", bucket, key)
        digest = hashlib.sha256(data).hexdigest()
        if digest != artifact.sha256:
            raise RuntimeError(f"sha256 mismatch: got {digest}, expected {artifact.sha256}")
        return OpResult(run_id, mode, phase, concurrency, "GET", bucket, key, len(data), time.perf_counter() - start, status, True)
    except Exception as e:
        return OpResult(run_id, mode, phase, concurrency, "GET", bucket, key, artifact.bytes, time.perf_counter() - start, 0, False, str(e))


def run_pool(fn, jobs: Iterable[Any], workers: int) -> list[OpResult]:
    if workers == 1:
        return [fn(job) for job in jobs]
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        return list(pool.map(fn, jobs))


def run_benchmark(args) -> int:
    run_id = args.run_id or slug_now()
    result_dir = Path(args.results) / run_id
    result_dir.mkdir(parents=True, exist_ok=True)
    artifacts = prepare_artifacts(
        data_dir=args.data_dir,
        artifact_count=args.artifact_count,
        artifact_extension=args.artifact_extension,
        artifact_source=args.artifact_source,
        alpine_branch=args.alpine_branch,
        alpine_arch=args.alpine_arch,
        alpine_flavor=args.alpine_flavor,
        reuse=args.reuse_artifacts,
    )
    endpoint = Endpoint("proxy", args.proxy_endpoint, args.access_key, args.secret_key, args.region)
    client = SigV4Client(endpoint, timeout=args.timeout)
    mode_buckets = parse_mode_buckets(args.mode_bucket)
    modes = args.modes or list(mode_buckets)
    concurrencies = [int(x) for x in args.concurrency.split(",")]

    (result_dir / "artifacts.json").write_text(json.dumps([asdict(a) for a in artifacts], indent=2) + "\n")
    (result_dir / "environment.json").write_text(json.dumps(capture_local_environment(), indent=2) + "\n")

    all_summary: dict[str, Any] = {"run_id": run_id, "modes": {}}
    for mode in modes:
        bucket = mode_buckets[mode]
        key_prefix = f"{args.prefix.rstrip('/')}/{run_id}/{mode}"
        all_summary["modes"].setdefault(mode, {})
        client.request("PUT", bucket, expected=(200, 409))
        for concurrency in concurrencies:
            print(f"mode={mode} bucket={bucket} concurrency={concurrency}")
            mode_dir = result_dir / mode / f"c{concurrency}"
            before = snapshot_proxy(args, f"before_{mode}_c{concurrency}", mode_dir)
            put_rows = run_pool(
                lambda artifact: put_object(client, bucket, f"{key_prefix}/{artifact.name}", artifact, mode, concurrency, run_id),
                artifacts,
                concurrency,
            )
            write_rows(mode_dir / "put.csv", put_rows)

            maybe_restart_proxy(args)
            cold_rows = run_pool(
                lambda artifact: get_object(client, bucket, f"{key_prefix}/{artifact.name}", artifact, mode, "cold_get", concurrency, run_id),
                artifacts,
                concurrency,
            )
            write_rows(mode_dir / "cold_get.csv", cold_rows)

            warm_rows = run_pool(
                lambda artifact: get_object(client, bucket, f"{key_prefix}/{artifact.name}", artifact, mode, "warm_get", concurrency, run_id),
                artifacts,
                concurrency,
            )
            write_rows(mode_dir / "warm_get.csv", warm_rows)

            after = snapshot_proxy(args, f"after_{mode}_c{concurrency}", mode_dir)
            all_summary["modes"][mode][f"c{concurrency}"] = {
                "put": summarize_ops(put_rows),
                "cold_get": summarize_ops(cold_rows),
                "warm_get": summarize_ops(warm_rows),
                "proxy_snapshot_before": before,
                "proxy_snapshot_after": after,
            }

    write_summary(result_dir / "summary.json", all_summary)
    write_markdown_report(result_dir / "report.md", all_summary)
    print(f"results: {result_dir}")
    failures = sum(
        phases[phase]["failed"]
        for by_conc in all_summary["modes"].values()
        for phases in by_conc.values()
        for phase in ("put", "cold_get", "warm_get")
    )
    if failures and not args.allow_failures:
        raise SystemExit(f"{failures} benchmark operations failed; see {result_dir}")
    return 0
