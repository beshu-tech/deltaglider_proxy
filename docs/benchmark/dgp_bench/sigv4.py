from __future__ import annotations

import datetime as dt
import hashlib
import hmac
import mmap
import ssl
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from .model import Endpoint


def _sha256_hex(body: bytes | mmap.mmap) -> str:
    if isinstance(body, mmap.mmap):
        body.seek(0)
        h = hashlib.sha256()
        while True:
            chunk = body.read(1024 * 1024)
            if not chunk:
                break
            h.update(chunk)
        return h.hexdigest()
    return hashlib.sha256(body).hexdigest()


class SigV4Client:
    def __init__(self, endpoint: Endpoint, timeout: float = 120.0) -> None:
        self.endpoint = endpoint
        self.timeout = timeout

    def url_for(self, bucket: str, key: str = "", query: str = "") -> str:
        quoted_key = "/".join(urllib.parse.quote(part, safe="") for part in key.split("/"))
        path = f"/{bucket}" + (f"/{quoted_key}" if key else "")
        return urllib.parse.urlunparse(
            (self.endpoint.parsed.scheme, self.endpoint.parsed.netloc, path, "", query, "")
        )

    def request(
        self,
        method: str,
        bucket: str,
        key: str = "",
        body: bytes = b"",
        extra_headers: dict[str, str] | None = None,
        query: str = "",
        expected: tuple[int, ...] = (200,),
    ) -> tuple[int, dict[str, str], bytes]:
        url = self.url_for(bucket, key, query)
        headers = self._sign(method, url, body, extra_headers or {})
        req = urllib.request.Request(
            url,
            data=body if method != "GET" else None,
            headers=headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=self.timeout, context=ssl.create_default_context()) as resp:
                data = resp.read()
                status = resp.status
                resp_headers = {k.lower(): v for k, v in resp.headers.items()}
        except urllib.error.HTTPError as e:
            data = e.read()
            status = e.code
            resp_headers = {k.lower(): v for k, v in e.headers.items()}
        if status not in expected:
            raise RuntimeError(f"{method} {url} returned {status}: {data[:200]!r}")
        return status, resp_headers, data

    def get_object_verify_stream(
        self,
        bucket: str,
        key: str,
        expected_sha256_hex: str,
        expected: tuple[int, ...] = (200,),
    ) -> tuple[int, float, int]:
        """Read GET body in chunks; verify SHA-256 without holding the full object in RAM."""
        url = self.url_for(bucket, key)
        headers = self._sign("GET", url, b"", {})
        req = urllib.request.Request(url, headers=headers, method="GET")
        wall0 = time.perf_counter()
        try:
            with urllib.request.urlopen(req, timeout=self.timeout, context=ssl.create_default_context()) as resp:
                status = resp.status
                if status not in expected:
                    _ = resp.read()
                    raise RuntimeError(f"GET {url} returned {status}")
                dig = hashlib.sha256()
                total = 0
                while True:
                    chunk = resp.read(1024 * 1024)
                    if not chunk:
                        break
                    dig.update(chunk)
                    total += len(chunk)
        except urllib.error.HTTPError as e:
            _ = e.read()
            raise RuntimeError(f"GET {url} returned {e.code}") from e
        wall_s = time.perf_counter() - wall0
        got = dig.hexdigest()
        if got != expected_sha256_hex:
            raise RuntimeError(f"sha256 mismatch: got {got}, expected {expected_sha256_hex}")
        return status, wall_s, total

    def put_object_file(
        self,
        bucket: str,
        key: str,
        file_path: Path,
        extra_headers: dict[str, str] | None = None,
        expected: tuple[int, ...] = (200,),
    ) -> tuple[int, dict[str, str], int]:
        """Upload file via mmap-backed buffer (hash + upload without ``read()`` into one giant ``bytes``)."""
        path = Path(file_path)
        with path.open("rb") as f:
            mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            url = self.url_for(bucket, key)
            eh = dict(extra_headers or {})
            eh.setdefault("Content-Type", "application/octet-stream")
            eh["Content-Length"] = str(len(mm))
            headers = self._sign_put_body(url, mm, eh)
            mm.seek(0)
            req = urllib.request.Request(url, data=mm, headers=headers, method="PUT")
            try:
                with urllib.request.urlopen(req, timeout=self.timeout, context=ssl.create_default_context()) as resp:
                    data = resp.read()
                    status = resp.status
                    resp_headers = {k.lower(): v for k, v in resp.headers.items()}
            except urllib.error.HTTPError as e:
                data = e.read()
                status = e.code
                resp_headers = {k.lower(): v for k, v in e.headers.items()}
            if status not in expected:
                raise RuntimeError(f"PUT {url} returned {status}: {data[:200]!r}")
            return status, resp_headers, len(mm)
        finally:
            mm.close()

    def _sign_put_body(self, url: str, body: mmap.mmap, extra_headers: dict[str, str]) -> dict[str, str]:
        parsed = urllib.parse.urlparse(url)
        now = dt.datetime.now(dt.UTC)
        amz_date = now.strftime("%Y%m%dT%H%M%SZ")
        date_stamp = now.strftime("%Y%m%d")
        payload_hash = _sha256_hex(body)
        signed_headers = {
            "host": parsed.netloc,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": amz_date,
            **{k.lower(): v.strip() for k, v in extra_headers.items()},
        }
        method = "PUT"
        canonical_headers = "".join(f"{k}:{signed_headers[k]}\n" for k in sorted(signed_headers))
        signed_header_names = ";".join(sorted(signed_headers))
        canonical_request = "\n".join(
            [
                method,
                parsed.path or "/",
                self._canonical_query(parsed.query),
                canonical_headers,
                signed_header_names,
                payload_hash,
            ]
        )
        scope = f"{date_stamp}/{self.endpoint.region}/s3/aws4_request"
        string_to_sign = "\n".join(
            [
                "AWS4-HMAC-SHA256",
                amz_date,
                scope,
                hashlib.sha256(canonical_request.encode()).hexdigest(),
            ]
        )
        signature = hmac.new(self._signing_key(date_stamp), string_to_sign.encode(), hashlib.sha256).hexdigest()
        auth = (
            "AWS4-HMAC-SHA256 "
            f"Credential={self.endpoint.access_key}/{scope},"
            f"SignedHeaders={signed_header_names},Signature={signature}"
        )
        return {
            **extra_headers,
            "Host": parsed.netloc,
            "x-amz-date": amz_date,
            "x-amz-content-sha256": payload_hash,
            "Authorization": auth,
        }

    def _sign(self, method: str, url: str, body: bytes, headers: dict[str, str]) -> dict[str, str]:
        parsed = urllib.parse.urlparse(url)
        now = dt.datetime.now(dt.UTC)
        amz_date = now.strftime("%Y%m%dT%H%M%SZ")
        date_stamp = now.strftime("%Y%m%d")
        payload_hash = hashlib.sha256(body).hexdigest()
        signed_headers = {
            "host": parsed.netloc,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": amz_date,
            **{k.lower(): v.strip() for k, v in headers.items()},
        }
        canonical_headers = "".join(f"{k}:{signed_headers[k]}\n" for k in sorted(signed_headers))
        signed_header_names = ";".join(sorted(signed_headers))
        canonical_request = "\n".join(
            [
                method,
                parsed.path or "/",
                self._canonical_query(parsed.query),
                canonical_headers,
                signed_header_names,
                payload_hash,
            ]
        )
        scope = f"{date_stamp}/{self.endpoint.region}/s3/aws4_request"
        string_to_sign = "\n".join(
            [
                "AWS4-HMAC-SHA256",
                amz_date,
                scope,
                hashlib.sha256(canonical_request.encode()).hexdigest(),
            ]
        )
        signature = hmac.new(self._signing_key(date_stamp), string_to_sign.encode(), hashlib.sha256).hexdigest()
        auth = (
            "AWS4-HMAC-SHA256 "
            f"Credential={self.endpoint.access_key}/{scope},"
            f"SignedHeaders={signed_header_names},Signature={signature}"
        )
        return {
            **headers,
            "Host": parsed.netloc,
            "x-amz-date": amz_date,
            "x-amz-content-sha256": payload_hash,
            "Authorization": auth,
        }

    def _signing_key(self, date_stamp: str) -> bytes:
        def sign(key: bytes, msg: str) -> bytes:
            return hmac.new(key, msg.encode(), hashlib.sha256).digest()

        k_date = sign(("AWS4" + self.endpoint.secret_key).encode(), date_stamp)
        k_region = sign(k_date, self.endpoint.region)
        k_service = sign(k_region, "s3")
        return sign(k_service, "aws4_request")

    @staticmethod
    def _canonical_query(query: str) -> str:
        pairs = urllib.parse.parse_qsl(query, keep_blank_values=True)
        return "&".join(
            f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}"
            for k, v in sorted(pairs)
        )
