# How to diagnose a backend that isn't serving

Follow this when a bucket answers `503 ServiceUnavailable`, the Backends panel shows a red health badge, or a boot log says a backend is UNHEALTHY. The proxy probes every configured backend's connectivity and credentials — at boot, on every config change to a backend, and continuously while one is down — so a broken backend always names itself and its cause.

## Read the verdict

Open **Settings → Storage → Backends**. Each backend carries a live health badge:

| Badge | Meaning | Fix |
|---|---|---|
| **Connected** | An authenticated call succeeded | Nothing to do |
| **Credentials rejected** | The backend answered and refused the key/secret | Check `access_key_id` / `secret_access_key` (typo, rotated key, unset `${env:...}` variable) |
| **Unreachable** | DNS / connect / TLS / timeout failure | Check `endpoint`, network egress, firewall |
| **Erroring** | Reachable but answering 5xx | The provider is degraded — wait or check their status page. Requests are **not** blocked for this state (the backend still serves); only *Credentials rejected* and *Unreachable* gate requests |

The same cause string appears verbatim in the boot log ERROR line, in the `503` body clients receive, and in a rejected config apply — they can never disagree.

## Force a probe now

Click **Test connection** on the backend card. This runs the probe **server-side**, with the server's actual credentials, endpoint, and network — a green result means the proxy itself can serve from this backend, not merely that your browser can reach it. Buckets gated by an unhealthy verdict reopen automatically within ~30 seconds of recovery; Test connection reopens them immediately.

## What happens while a backend is down

- Every request to a bucket routed to it answers a fast `503 ServiceUnavailable` naming the backend and cause — no per-request timeouts, no misleading `404`s, and the file browser shows the fault instead of an empty bucket.
- Buckets on healthy backends are unaffected.
- The proxy re-probes the unhealthy backend every 30 seconds and logs the recovery.

## Boot behaviour

At startup the proxy probes every configured backend (the default plus each entry in `storage.backends`):

- **Some fail** → the proxy starts **degraded**: an ERROR log per backend, their buckets gated with 503s.
- **All fail** → the proxy **refuses to start** (exit code 1) — a proxy with zero working storage serves nothing but errors.

```bash
# Relax the gate if you must boot against a temporarily-dark backend:
DGP_BOOT_BACKEND_PROBE=warn   # probe + log, never exit
DGP_BOOT_BACKEND_PROBE=off    # skip probing entirely
```

Scoped keys are handled: a key restricted to one bucket (e.g. a Backblaze B2 application key) legitimately cannot list buckets, so the probe falls back to a `HeadBucket` on a bucket routed to that backend before concluding anything about the credentials.

## Config changes are probed too

Applying a config that **changes a backend's definition** (endpoint, credentials) runs the probe first. A failing probe rejects the apply with the cause, and nothing changes:

```text
config refused: backend 'aws-dr' failed its connection probe — credentials
rejected (status=403 code=InvalidAccessKeyId) — check access_key_id /
secret_access_key. Fix the endpoint/credentials and re-apply (nothing was changed)
```

Two related states are **fatal config errors** (refused at boot and on every apply): a bucket routed to a backend name that doesn't exist, and duplicate backend names.

## Related

- [Backend capability validation](backend-capability-validation.md) — the *other* backend gate: conditional-write (CAS) support for multi-instance safety.
- [Route a bucket to a backend](route-a-bucket-to-a-backend.md)
- [Configuration reference](../reference/configuration.md) — `DGP_BOOT_BACKEND_PROBE`.
