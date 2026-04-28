from __future__ import annotations

import argparse
import os
import urllib.request

from .artifacts import prepare_artifacts
from .config import DEFAULT_MODES, add_artifact_args, slug_now
from .hcloud_lifecycle import down, status, up
from .runner import run_benchmark
from .single_vm import smoke as single_vm_smoke
from .html_report import generate_html_report


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(description="DeltaGlider production compression-tax benchmark")
    sub = p.add_subparsers(dest="command", required=True)

    for name in ["up", "status", "down"]:
        sp = sub.add_parser(name)
        sp.add_argument("--run-id", default=os.environ.get("DGP_BENCH_RUN_ID", slug_now()))
    up_cmd = sub.choices["up"]
    up_cmd.add_argument("--location", default="fsn1")
    up_cmd.add_argument("--image", default="ubuntu-24.04")
    up_cmd.add_argument("--client-type", default="cpx31")
    up_cmd.add_argument("--proxy-type", default="cpx31")
    up_cmd.add_argument("--ssh-key-name")
    up_cmd.add_argument(
        "--single-vm",
        action="store_true",
        help="Create one all-in-one debug VM instead of separate client/proxy VMs.",
    )
    sub.choices["down"].add_argument("--dry-run", action="store_true")

    doctor = sub.add_parser("doctor")
    doctor.add_argument("--proxy-endpoint")
    doctor.add_argument("--hcloud", action="store_true")
    doctor.add_argument("--run-id", default=os.environ.get("DGP_BENCH_RUN_ID", slug_now()))

    artifacts = sub.add_parser("artifacts")
    add_artifact_args(artifacts)

    smoke = sub.add_parser("single-vm-smoke")
    smoke.add_argument("--run-id", required=True)
    smoke.add_argument("--artifact-count", type=int, default=5)
    smoke.add_argument("--artifact-source", choices=["kernel", "alpine-iso"], default="alpine-iso")
    smoke.add_argument("--artifact-extension", default=".iso")
    smoke.add_argument("--alpine-branch", default="v3.19")
    smoke.add_argument("--alpine-arch", default="x86_64")
    smoke.add_argument("--alpine-flavor", default="virt")
    smoke.add_argument("--concurrency", default="1")

    run = sub.add_parser("run")
    add_artifact_args(run)
    run.add_argument("--proxy-endpoint", required=True)
    run.add_argument("--access-key", default=os.environ.get("DGP_BENCH_ACCESS_KEY", ""))
    run.add_argument("--secret-key", default=os.environ.get("DGP_BENCH_SECRET_KEY", ""))
    run.add_argument("--region", default=os.environ.get("DGP_BENCH_REGION", "us-east-1"))
    run.add_argument("--mode-bucket", action="append", help="MODE=BUCKET override")
    run.add_argument("--modes", nargs="+", choices=list(DEFAULT_MODES))
    run.add_argument("--prefix", default="bench")
    run.add_argument("--run-id", default=os.environ.get("DGP_BENCH_RUN_ID", slug_now()))
    run.add_argument("--concurrency", default="1,4")
    run.add_argument("--results", default="results")
    run.add_argument("--timeout", type=float, default=300)
    run.add_argument("--metrics-url")
    run.add_argument("--stats-url")
    run.add_argument("--health-url")
    run.add_argument("--restart-command", help="Optional shell command to clear cache/restart proxy before cold GET")
    run.add_argument("--restart-timeout", type=float, default=120)
    run.add_argument("--allow-failures", action="store_true")

    html_rep = sub.add_parser("html-report")
    html_rep.add_argument("--bundle", required=True, help="Extracted results directory or .tgz bundle")
    html_rep.add_argument("--out", required=True, help="Output HTML path")
    return p


def doctor(args) -> int:
    errors = 0
    if args.proxy_endpoint:
        try:
            with urllib.request.urlopen(args.proxy_endpoint.rstrip("/") + "/_/health", timeout=10) as resp:
                print("proxy health", resp.status)
        except Exception as e:
            errors += 1
            print("proxy health ERROR", e)
    if args.hcloud:
        try:
            return status(args)
        except Exception as e:
            errors += 1
            print("hcloud ERROR", e)
    return 1 if errors else 0


def main() -> int:
    args = build_parser().parse_args()
    if args.command == "up":
        return up(args)
    if args.command == "status":
        return status(args)
    if args.command == "down":
        return down(args)
    if args.command == "doctor":
        return doctor(args)
    if args.command == "artifacts":
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
        print(f"prepared {len(artifacts)} artifacts, {sum(a.bytes for a in artifacts)/1e9:.2f} GB")
        return 0
    if args.command == "single-vm-smoke":
        return single_vm_smoke(args)
    if args.command == "run":
        if not args.access_key or not args.secret_key:
            raise SystemExit("--access-key/--secret-key or DGP_BENCH_ACCESS_KEY/DGP_BENCH_SECRET_KEY required")
        return run_benchmark(args)
    if args.command == "html-report":
        generate_html_report(args.bundle, args.out)
        print(f"wrote {args.out}")
        return 0
    raise AssertionError(args.command)
