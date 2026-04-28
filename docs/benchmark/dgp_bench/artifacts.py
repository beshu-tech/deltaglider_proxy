from __future__ import annotations

import json
import os
import re
import shutil
import urllib.parse
import urllib.request
from dataclasses import asdict
from pathlib import Path

from .config import DEFAULT_KERNEL_INDEX
from .model import Artifact
from .util import sha256_file


def fetch_url(url: str, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    with urllib.request.urlopen(url, timeout=60) as resp, tmp.open("wb") as out:
        shutil.copyfileobj(resp, out)
    tmp.replace(dest)


def _norm_extension(extension: str) -> str:
    return extension if extension.startswith(".") else f".{extension}"


def _version_key(version: str) -> tuple[int, ...]:
    return tuple(int(part) for part in version.split("."))


def list_kernel_artifacts(limit: int, extension: str) -> list[tuple[str, str]]:
    index = os.environ.get("DGP_BENCH_KERNEL_INDEX", DEFAULT_KERNEL_INDEX)
    with urllib.request.urlopen(index, timeout=30) as resp:
        html = resp.read().decode("utf-8", "replace")
    found: dict[str, str] = {}
    ext = _norm_extension(extension)
    pattern = rf'href="(linux-(\d+\.\d+(?:\.\d+)?){re.escape(ext)})"'
    for filename, version in re.findall(pattern, html):
        found[version] = urllib.parse.urljoin(index, filename)
    versions = sorted(found, key=_version_key)
    return [(f"linux-{v}{ext}", found[v]) for v in versions[-limit:]]


def list_alpine_iso_artifacts(
    limit: int,
    extension: str,
    branch: str,
    arch: str,
    flavor: str,
) -> list[tuple[str, str]]:
    ext = _norm_extension(extension)
    if ext != ".iso":
        raise SystemExit("--artifact-source alpine-iso currently requires --artifact-extension .iso")
    default_index = f"https://dl-cdn.alpinelinux.org/alpine/{branch}/releases/{arch}/"
    index = os.environ.get("DGP_BENCH_ALPINE_INDEX", default_index)
    with urllib.request.urlopen(index, timeout=30) as resp:
        html = resp.read().decode("utf-8", "replace")
    found: dict[str, str] = {}
    pattern = rf'href="(alpine-{re.escape(flavor)}-(\d+\.\d+\.\d+)-{re.escape(arch)}{re.escape(ext)})"'
    for filename, version in re.findall(pattern, html):
        found[version] = urllib.parse.urljoin(index, filename)
    versions = sorted(found, key=_version_key)
    return [(f"alpine-{flavor}-{v}-{arch}{ext}", found[v]) for v in versions[-limit:]]


def list_artifacts(
    source: str,
    limit: int,
    extension: str,
    alpine_branch: str,
    alpine_arch: str,
    alpine_flavor: str,
) -> list[tuple[str, str]]:
    if source == "kernel":
        return list_kernel_artifacts(limit, extension)
    if source == "alpine-iso":
        return list_alpine_iso_artifacts(limit, extension, alpine_branch, alpine_arch, alpine_flavor)
    raise SystemExit(f"unsupported --artifact-source {source!r}")


def prepare_artifacts(
    data_dir: str,
    artifact_count: int,
    artifact_extension: str,
    artifact_source: str,
    alpine_branch: str,
    alpine_arch: str,
    alpine_flavor: str,
    reuse: bool,
) -> list[Artifact]:
    root = Path(data_dir)
    root.mkdir(parents=True, exist_ok=True)
    manifest_path = root / "manifest.json"
    if reuse and manifest_path.exists():
        return [Artifact(**x) for x in json.loads(manifest_path.read_text())]

    artifacts: list[Artifact] = []
    for name, url in list_artifacts(
        source=artifact_source,
        limit=artifact_count,
        extension=artifact_extension,
        alpine_branch=alpine_branch,
        alpine_arch=alpine_arch,
        alpine_flavor=alpine_flavor,
    ):
        path = root / name
        if not path.exists():
            print(f"fetch {name}")
            fetch_url(url, path)
        artifacts.append(
            Artifact(
                name=name,
                path=str(path),
                bytes=path.stat().st_size,
                sha256=sha256_file(path),
                source_url=url,
            )
        )
    manifest_path.write_text(json.dumps([asdict(a) for a in artifacts], indent=2) + "\n")
    return artifacts
