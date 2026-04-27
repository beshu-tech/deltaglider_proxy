from __future__ import annotations

import urllib.parse
from dataclasses import dataclass


@dataclass(frozen=True)
class Endpoint:
    name: str
    url: str
    access_key: str
    secret_key: str
    region: str = "us-east-1"

    @property
    def parsed(self) -> urllib.parse.ParseResult:
        return urllib.parse.urlparse(self.url.rstrip("/"))


@dataclass(frozen=True)
class Artifact:
    name: str
    path: str
    bytes: int
    sha256: str
    source_url: str


@dataclass
class OpResult:
    run_id: str
    mode: str
    phase: str
    concurrency: int
    op: str
    bucket: str
    key: str
    bytes: int
    wall_s: float
    status: int
    ok: bool
    error: str = ""
