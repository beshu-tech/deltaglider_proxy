from __future__ import annotations

import argparse
import datetime as dt
import os
from typing import Final

APP = "dgp-compression-tax-bench"
# Hetzner Cloud defaults — Helsinki (`hel1`) + type that provisions there (`cpx31` is often unavailable in EU).
DEFAULT_HC_LOCATION = "hel1"
DEFAULT_HC_SERVER_TYPE = "ccx33"
DEFAULT_KERNEL_INDEX = "https://cdn.kernel.org/pub/linux/kernel/v6.x/"
DEFAULT_ALPINE_RELEASE_BRANCH = "v3.19"
DEFAULT_ALPINE_ARCH = "x86_64"
DEFAULT_ALPINE_FLAVOR = "virt"
DEFAULT_MODES = {
    "passthrough": "bench-passthrough",
    "compression": "bench-compression",
    "encryption": "bench-encryption",
    "compression_encryption": "bench-compression-encryption",
}

# Canonical benchmark mode sequence (charts, tables, markdown). Same keys as DEFAULT_MODES.
MODE_ORDER: Final[tuple[str, ...]] = tuple(DEFAULT_MODES.keys())

MODE_BAND_COLORS: Final[dict[str, str]] = {
    "passthrough": "rgba(148, 163, 184, 0.15)",
    "compression": "rgba(59, 130, 246, 0.18)",
    "encryption": "rgba(239, 68, 68, 0.12)",
    "compression_encryption": "rgba(168, 85, 247, 0.16)",
}

MODE_SHORT_LABELS: Final[dict[str, str]] = {
    "passthrough": "Passthrough",
    "compression": "Compression",
    "encryption": "Encryption",
    "compression_encryption": "Comp + encrypt",
}

REPORT_THROUGHPUT_HINTS: Final[dict[str, str]] = {
    "passthrough": "Benchmark mode: compression off on this bucket (baseline). Not evaluating xdelta here — unrelated to whether .iso is eligible elsewhere.",
    "compression": "Compression mode: xdelta on; plain backend; CPU-heavy PUTs. Default FileRouter treats .iso as delta-eligible.",
    "encryption": "Proxy AES-256-GCM; ciphertext stored under encrypted backend root.",
    "compression_encryption": "Both delta encoding and proxy encryption active.",
}

REPORT_STORAGE_HINTS: Final[dict[str, str]] = {
    "passthrough": "Prom Δ usually 0 for this segment unless counters moved between snapshots.",
    "compression": "Δ saved reflects bytes attributed to delta storage during this mode’s benchmark slice.",
    "encryption": "Typically little/no Δ saved (no delta); implied stored ≈ logical.",
    "compression_encryption": "Combined effect: delta savings where applicable + encrypted backend footprint.",
}


def utc_now() -> str:
    return dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat()


def slug_now() -> str:
    return dt.datetime.now(dt.UTC).strftime("%Y%m%dT%H%M%SZ")


def parse_mode_buckets(items: list[str] | None) -> dict[str, str]:
    out = dict(DEFAULT_MODES)
    if not items:
        return out
    for item in items:
        if "=" not in item:
            raise SystemExit(f"--mode-bucket must be MODE=BUCKET, got {item!r}")
        mode, bucket = item.split("=", 1)
        if mode not in out:
            raise SystemExit(f"unknown mode {mode!r}; expected one of {sorted(out)}")
        out[mode] = bucket
    return out


def add_artifact_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--data-dir", default="data/artifacts")
    p.add_argument("--artifact-count", type=int, default=20)
    p.add_argument(
        "--artifact-source",
        choices=["kernel", "alpine-iso"],
        default="kernel",
        help="Artifact source family to benchmark.",
    )
    p.add_argument(
        "--artifact-extension",
        default=".tar.xz",
        help="Artifact extension filter, e.g. .tar.gz (kernel) or .iso (alpine-iso).",
    )
    p.add_argument(
        "--alpine-branch",
        default=DEFAULT_ALPINE_RELEASE_BRANCH,
        help="Alpine release branch when using --artifact-source alpine-iso (for example v3.19).",
    )
    p.add_argument(
        "--alpine-arch",
        default=DEFAULT_ALPINE_ARCH,
        help="Alpine architecture when using --artifact-source alpine-iso.",
    )
    p.add_argument(
        "--alpine-flavor",
        default=DEFAULT_ALPINE_FLAVOR,
        help="Alpine image flavor when using --artifact-source alpine-iso (for example virt).",
    )
    p.add_argument("--reuse-artifacts", action="store_true")


def env(name: str, default: str = "") -> str:
    return os.environ.get(name, default)
