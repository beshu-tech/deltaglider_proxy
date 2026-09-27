# How to diagnose a backend that isn't serving

Follow this when a bucket answers `503 ServiceUnavailable`, the Backends panel shows a red health badge, or a boot log says a backend is UNHEALTHY. The proxy probes every configured backend's connectivity and credentials at boot, on every config change to a backend, and every 30 seconds after that (healthy backends too). A request that finds a backend unavailable also marks it unhealthy. Because of this, a broken backend always reports its name and its cause.

## Read the verdict

Open **Settings → Storage → Backends**. Each backend carries a live health badge:

| Badge | Meaning | Fix |
|---|---|---|
| **Connected** | An authenticated request succeeded | Nothing to do |
| **Credentials rejected** | The backend answered and refused the key/secret | Check `access_key_id` / `secret_access_key` (typo, rotated key, unset `${env:...}` variable) |
| **Unreachable** | DNS / connect / TLS / timeout failure | Check `endpoint`, network egress, firewall |
| **Erroring** | Reachable but answering 5xx | The provider is degraded. Wait, or check the provider's status page. The proxy does **not** block requests in this state, because the backend still serves. Only *Credentials rejected* and *Unreachable* block requests |

The same cause string appears verbatim in the boot log ERROR line, in the `503` body clients receive, and in a rejected config apply, so these places always agree.

## Force a probe now

Click **Test connection** on the backend card. This runs the probe on the server, with the server's own credentials, endpoint, and network. A green result means that the proxy itself can serve from this backend, which is more than your browser being able to reach it. Buckets that an unhealthy verdict blocks reopen automatically within about 30 seconds of recovery, and Test connection reopens them immediately.

## What happens while a backend is down

- Every request to a bucket routed to it gets a fast `503 ServiceUnavailable` that names the backend and the cause. Requests do not wait for timeouts, clients do not get misleading `404`s, and the file browser shows the fault instead of an empty bucket.
- Buckets on healthy backends are unaffected.
- The proxy probes every backend every 30 seconds (`DGP_BACKEND_HEALTH_INTERVAL_SECS`) and logs the recovery.

## What happens when a backend hangs

A backend can accept connections and then never answer, for example when its process is paused. The proxy does not wait for such a backend for minutes. Each backend request that carries no large body (HEAD, GET until the first byte, LIST, DELETE) has a deadline of 30 seconds, retries included (`DGP_BACKEND_REQUEST_TIMEOUT_SECS`). When the deadline passes, the client gets `503 ServiceUnavailable` with the backend's name, and the proxy marks the backend **Unreachable** at once. The next requests to the backend's buckets then get the 503 immediately, without a wait. The health probe also finds a hang on its own, because it probes healthy backends too. When the backend answers a probe again, its buckets reopen. `GET /_/ready` lists each backend's live state in its `backends` field.

## Boot behaviour

At startup the proxy probes every configured backend (the default plus each entry in `storage.backends`):

- If some backends fail, the proxy starts **degraded**. It logs one ERROR line per failed backend, and the buckets of those backends answer with 503s.
- If all backends fail, the proxy **refuses to start** (exit code 1), because a proxy with no working storage can only serve errors.

```bash
# Relax the gate if you must boot against a temporarily-dark backend:
DGP_BOOT_BACKEND_PROBE=warn   # probe + log, never exit
DGP_BOOT_BACKEND_PROBE=off    # skip probing entirely
```

The probe handles scoped keys. A key restricted to one bucket (e.g. a Backblaze B2 application key) legitimately cannot list buckets, so the probe falls back to a `HeadBucket` on a bucket routed to that backend before concluding anything about the credentials.

## Config changes are probed too

When you apply a config that changes a backend's definition (endpoint, credentials), the proxy runs the probe first. A failing probe rejects the apply with the cause, and nothing changes:

```text
config refused: backend 'aws-dr' failed its connection probe — credentials
rejected (status=403 code=InvalidAccessKeyId) — check access_key_id /
secret_access_key. Fix the endpoint/credentials and re-apply (nothing was changed)
```

Two related states are fatal config errors, which the proxy refuses at boot and on every apply: a bucket routed to a backend name that doesn't exist, and duplicate backend names.

## Related

- [Backend capability validation](backend-capability-validation.md): the other backend check, for conditional-write (CAS) support in multi-instance deployments.
- [Route a bucket to a backend](route-a-bucket-to-a-backend.md)
- [Configuration reference](../reference/configuration.md): `DGP_BOOT_BACKEND_PROBE`.
