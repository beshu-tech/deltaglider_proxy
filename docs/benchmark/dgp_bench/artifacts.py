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


def list_kernel_artifacts(limit: int, extension: str) -> list[tuple[str, str]]:
    index = os.environ.get("DGP_BENCH_KERNEL_INDEX", DEFAULT_KERNEL_INDEX)
    with urllib.request.urlopen(index, timeout=30) as resp:
        html = resp.read().decode("utf-8", "replace")
    found: dict[str, str] = {}
    escaped_ext = re.escape(extension.lstrip("."))
    pattern = rf'href="(linux-(\d+\.\d+(?:\.\d+)?)\.{escaped_ext})"'
    for filename, version in re.findall(pattern, html):
        found[version] = urllib.parse.urljoin(index, filename)
    versions = sorted(found, key=lambda v: tuple(int(part) for part in v.split(".")))
    return [(f"linux-{v}.tar.xz", found[v]) for v in versions[-limit:]]


def prepare_artifacts(data_dir: str, artifact_count: int, artifact_extension: str, reuse: bool) -> list[Artifact]:
    root = Path(data_dir)
    root.mkdir(parents=True, exist_ok=True)
    manifest_path = root / "manifest.json"
    if reuse and manifest_path.exists():
        return [Artifact(**x) for x in json.loads(manifest_path.read_text())]

    artifacts: list[Artifact] = []
    for name, url in list_kernel_artifacts(artifact_count, artifact_extension):
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
