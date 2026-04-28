from __future__ import annotations

import concurrent.futures
import json
import os
import subprocess
import threading
import time
from dataclasses import asdict
from pathlib import Path
from typing import Any, Iterable

from .artifacts import prepare_artifacts, sort_artifacts_baseline_first
from .compression_verify import verify_compression_modes_recorded_delta
from .bench_summary import count_phase_failures
from .config import parse_mode_buckets, slug_now
from .metrics import collect_resource_sample, maybe_restart_proxy, restart_proxy_command, snapshot_proxy
from .model import Artifact, Endpoint, OpResult
from .reporting import summarize_ops, write_markdown_report, write_rows, write_summary
from .resources_rollup import build_resources_rollup
from .sigv4 import SigV4Client
from .trial_aggregate import build_trial_aggregate, write_aggregate, write_aggregate_markdown
from .util import capture_local_environment


def _maybe_phase_gap(args: Any) -> None:
    """Idle between phases so resource timeseries shows CPU/memory dips (optional)."""
    g = float(getattr(args, "phase_gap_seconds", 0.0) or 0.0)
    if g > 0:
        time.sleep(g)


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
        status, _, _ = client.put_object_file(
            bucket,
            key,
            Path(artifact.path),
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
        status, _wall, nbytes = client.get_object_verify_stream(bucket, key, artifact.sha256)
        return OpResult(run_id, mode, phase, concurrency, "GET", bucket, key, nbytes, time.perf_counter() - start, status, True)
    except Exception as e:
        return OpResult(run_id, mode, phase, concurrency, "GET", bucket, key, artifact.bytes, time.perf_counter() - start, 0, False, str(e))


def run_pool(fn, jobs: Iterable[Any], workers: int) -> list[OpResult]:
    if workers == 1:
        return [fn(job) for job in jobs]
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        return list(pool.map(fn, jobs))


def put_objects_delta_order(
    put_one,
    artifacts: list[Artifact],
    concurrency: int,
) -> list[OpResult]:
    """Upload first object alone so it wins the deltaspace reference race; then parallelize."""
    if not artifacts:
        return []
    first = put_one(artifacts[0])
    if len(artifacts) == 1:
        return [first]
    rest = run_pool(put_one, artifacts[1:], concurrency)
    return [first, *rest]


def _allow_clean_failure(args: Any) -> bool:
    if getattr(args, "allow_clean_failure", False):
        return True
    return os.environ.get("DGP_BENCH_ALLOW_CLEAN_FAILURE", "").lower() in ("1", "true", "yes")


def _merge_multi_trial_parent_summary(
    parent_run_id: str,
    primary_summary: dict[str, Any],
    trial_directories: list[str],
    *,
    trial_count_expected: int,
    trial_count_completed: int,
) -> dict[str, Any]:
    out = dict(primary_summary)
    out["run_id"] = parent_run_id
    out["multi_trial"] = True
    out["trial_count"] = len(trial_directories)
    out["trial_count_expected"] = trial_count_expected
    out["trial_count_completed"] = trial_count_completed
    out["trial_directories"] = trial_directories
    out["aggregate_path"] = "aggregate.json"
    out["primary_trial_dir"] = "trial_001"
    ts = primary_summary.get("resource_timeseries_path")
    if ts and not str(ts).startswith("trial_"):
        out["resource_timeseries_path"] = f"trial_001/{ts}"
    return out


def _execute_single_benchmark_run(
    args: Any,
    result_dir: Path,
    run_id: str,
    artifacts: list[Artifact],
) -> dict[str, Any]:
    result_dir.mkdir(parents=True, exist_ok=True)
    endpoint = Endpoint("proxy", args.proxy_endpoint, args.access_key, args.secret_key, args.region)
    client = SigV4Client(endpoint, timeout=args.timeout)
    mode_buckets = parse_mode_buckets(args.mode_bucket)
    modes = args.modes or list(mode_buckets)
    concurrencies = [int(x) for x in args.concurrency.split(",")]

    artifacts = sort_artifacts_baseline_first(list(artifacts))

    resource_samples: list[dict[str, Any]] = []
    resource_markers: list[dict[str, Any]] = []
    sample_interval_s = float(getattr(args, "resource_sample_interval", 2.0) or 0.0)
    should_sample = sample_interval_s > 0 and bool(args.metrics_url or args.health_url or args.resource_command)
    sampler_stop = threading.Event()
    sampler_thread: threading.Thread | None = None
    sample_t0 = time.time()

    def _mark(event: str, mode: str = "", concurrency: int = 0, phase: str = "") -> None:
        resource_markers.append(
            {
                "event": event,
                "timestamp": time.time(),
                "elapsed_s": round(time.time() - sample_t0, 3),
                "mode": mode,
                "concurrency": concurrency,
                "phase": phase,
            }
        )

    def _capture_sample(tag: str = "") -> None:
        sample = collect_resource_sample(args)
        sample["elapsed_s"] = round(time.time() - sample_t0, 3)
        if tag:
            sample["tag"] = tag
        resource_samples.append(sample)

    if should_sample:
        _mark("sampling_start")
        _capture_sample("start")

        def _sampling_loop() -> None:
            while not sampler_stop.wait(sample_interval_s):
                _capture_sample()

        sampler_thread = threading.Thread(target=_sampling_loop, name="dgp-bench-resource-sampler", daemon=True)
        sampler_thread.start()

    modes_list = list(modes)
    all_summary: dict[str, Any] = {"run_id": run_id, "modes": {}}
    try:
        for mi, mode in enumerate(modes_list):
            bucket = mode_buckets[mode]
            key_prefix = f"{args.prefix.rstrip('/')}/{run_id}/{mode}"
            all_summary["modes"].setdefault(mode, {})
            client.request("PUT", bucket, expected=(200, 409))
            for concurrency in concurrencies:
                print(f"mode={mode} bucket={bucket} concurrency={concurrency}")
                mode_dir = result_dir / mode / f"c{concurrency}"
                _mark("before_snapshot", mode, concurrency)
                before = snapshot_proxy(args, f"before_{mode}_c{concurrency}", mode_dir)
                _mark("phase_start", mode, concurrency, "put")
                t_put = time.perf_counter()

                def _put(artifact: Artifact) -> OpResult:
                    return put_object(
                        client,
                        bucket,
                        f"{key_prefix}/{artifact.name}",
                        artifact,
                        mode,
                        concurrency,
                        run_id,
                    )

                put_rows = put_objects_delta_order(_put, artifacts, concurrency)
                wall_put = time.perf_counter() - t_put
                _mark("phase_end", mode, concurrency, "put")
                write_rows(mode_dir / "put.csv", put_rows)
                _maybe_phase_gap(args)

                maybe_restart_proxy(args)
                _mark("phase_start", mode, concurrency, "cold_get")
                t_cold = time.perf_counter()
                cold_rows = run_pool(
                    lambda artifact: get_object(client, bucket, f"{key_prefix}/{artifact.name}", artifact, mode, "cold_get", concurrency, run_id),
                    artifacts,
                    concurrency,
                )
                wall_cold = time.perf_counter() - t_cold
                _mark("phase_end", mode, concurrency, "cold_get")
                write_rows(mode_dir / "cold_get.csv", cold_rows)
                _maybe_phase_gap(args)

                _mark("phase_start", mode, concurrency, "warm_get")
                t_warm = time.perf_counter()
                warm_rows = run_pool(
                    lambda artifact: get_object(client, bucket, f"{key_prefix}/{artifact.name}", artifact, mode, "warm_get", concurrency, run_id),
                    artifacts,
                    concurrency,
                )
                wall_warm = time.perf_counter() - t_warm
                _mark("phase_end", mode, concurrency, "warm_get")
                write_rows(mode_dir / "warm_get.csv", warm_rows)
                _maybe_phase_gap(args)

                _mark("after_snapshot", mode, concurrency)
                after = snapshot_proxy(args, f"after_{mode}_c{concurrency}", mode_dir)
                all_summary["modes"][mode][f"c{concurrency}"] = {
                    "put": summarize_ops(put_rows, phase_wall_s=wall_put),
                    "cold_get": summarize_ops(cold_rows, phase_wall_s=wall_cold),
                    "warm_get": summarize_ops(warm_rows, phase_wall_s=wall_warm),
                    "proxy_snapshot_before": before,
                    "proxy_snapshot_after": after,
                }

            rbm = getattr(args, "restart_between_modes_command", None)
            if rbm and str(rbm).strip() and mi < len(modes_list) - 1:
                _mark("restart_between_modes", mode, 0, "")
                print(f"restart_between_modes: after mode={mode!r} (fresh process before next mode)")
                restart_proxy_command(rbm, args)
    finally:
        if should_sample:
            sampler_stop.set()
            if sampler_thread:
                sampler_thread.join(timeout=max(5.0, sample_interval_s * 2))
            _capture_sample("end")
            _mark("sampling_end")

    rollup = build_resources_rollup(result_dir)
    (result_dir / "resources_rollup.json").write_text(json.dumps(rollup, indent=2) + "\n")
    all_summary["resources_rollup"] = rollup
    if should_sample:
        timeseries = {
            "schema": "dgp-bench-resource-timeseries/v1",
            "sample_interval_s": sample_interval_s,
            "samples": resource_samples,
            "markers": resource_markers,
        }
        (result_dir / "resource_timeseries.json").write_text(json.dumps(timeseries, indent=2) + "\n")
        all_summary["resource_timeseries_path"] = "resource_timeseries.json"

    all_summary["restart_between_modes_command"] = (getattr(args, "restart_between_modes_command", None) or "").strip()
    write_summary(result_dir / "summary.json", all_summary)
    write_markdown_report(result_dir / "report.md", all_summary)
    verify_compression_modes_recorded_delta(result_dir, all_summary, args)
    print(f"trial results: {result_dir}")
    return all_summary


def run_benchmark(args: Any) -> int:
    trials = max(1, int(getattr(args, "trials", 1) or 1))
    parent_run_id = args.run_id or slug_now()
    base_results = Path(args.results)

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

    if trials == 1:
        result_dir = base_results / parent_run_id
        result_dir.mkdir(parents=True, exist_ok=True)
        (result_dir / "artifacts.json").write_text(json.dumps([asdict(a) for a in artifacts], indent=2) + "\n")
        (result_dir / "environment.json").write_text(json.dumps(capture_local_environment(), indent=2) + "\n")
        all_summary = _execute_single_benchmark_run(args, result_dir, parent_run_id, artifacts)
        print(f"results: {result_dir}")
        failures = count_phase_failures(all_summary)
        if failures and not args.allow_failures:
            raise SystemExit(f"{failures} benchmark operations failed; see {result_dir}")
        return 0

    parent_dir = base_results / parent_run_id
    parent_dir.mkdir(parents=True, exist_ok=True)
    (parent_dir / "artifacts.json").write_text(json.dumps([asdict(a) for a in artifacts], indent=2) + "\n")
    (parent_dir / "environment.json").write_text(json.dumps(capture_local_environment(), indent=2) + "\n")

    clean_cmd = (getattr(args, "clean_command", None) or "").strip()

    for i in range(trials):
        if i > 0 and clean_cmd:
            print(f"running clean-command before trial {i + 1}/{trials}...")
            r = subprocess.run(clean_cmd, shell=True)
            if r.returncode != 0:
                msg = f"clean-command exited with status {r.returncode}"
                if _allow_clean_failure(args):
                    print(f"warning: {msg} (--allow-clean-failure / DGP_BENCH_ALLOW_CLEAN_FAILURE)")
                else:
                    raise SystemExit(
                        f"{msg}; aborting (trials after a failed clean are confounded). "
                        "Retry with --allow-clean-failure if you intend to continue."
                    )

        trial_dir = parent_dir / f"trial_{i + 1:03d}"
        trial_run_id = f"{parent_run_id}-t{i + 1:03d}"
        _execute_single_benchmark_run(args, trial_dir, trial_run_id, artifacts)

    try:
        agg = build_trial_aggregate(parent_dir, trials_expected=trials)
    except ValueError as e:
        raise SystemExit(str(e)) from e
    write_aggregate(parent_dir, agg)
    write_aggregate_markdown(parent_dir, agg)

    trial_names = list(agg.get("trial_directories") or [])
    completed = int(agg.get("trial_count_completed", len(trial_names)))
    primary_path = parent_dir / "trial_001" / "summary.json"
    if not primary_path.exists():
        raise SystemExit(f"missing {primary_path}; multi-trial run did not produce trial_001")
    primary_summary = json.loads(primary_path.read_text())
    parent_summary = _merge_multi_trial_parent_summary(
        parent_run_id,
        primary_summary,
        trial_names,
        trial_count_expected=trials,
        trial_count_completed=completed,
    )
    write_summary(parent_dir / "summary.json", parent_summary)
    write_markdown_report(parent_dir / "report.md", parent_summary)

    print(f"results: {parent_dir}")
    failures = 0
    for j in range(trials):
        sp = parent_dir / f"trial_{j + 1:03d}" / "summary.json"
        if sp.exists():
            failures += count_phase_failures(json.loads(sp.read_text()))
    if failures and not args.allow_failures:
        raise SystemExit(f"{failures} benchmark operations failed across trials; see {parent_dir}")
    return 0
