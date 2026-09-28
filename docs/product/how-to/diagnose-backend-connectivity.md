# How to diagnose a backend that isn't serving

Follow this when a bucket answers `503 ServiceUnavailable`, the Backends panel shows a red health badge, or a boot log says a backend is UNHEALTHY. The proxy probes every configured backend's connectivity and credentials at boot, on every config change to a backend, and every 30 seconds after that (healthy backends too). A request that finds a backend unavailable also marks it unhealthy. Because of this, a broken backend always reports its name and its cause.

## Read the verdict

In the sidebar of the admin UI, open **Storage → Backends** (`/_/admin/storage/backends`). Each backend card carries a live health badge next to the name of the backend:

| Badge | Meaning | Fix |
|---|---|---|
| **Connected** | An authenticated request succeeded. | Nothing to do. |
| **CREDENTIALS REJECTED** | The backend answered and refused the access key or the secret. | Check `access_key_id` and `secret_access_key`: a typo, a rotated key, or an unset `${env:...}` variable. |
| **UNREACHABLE** | The request failed at DNS, connect, TLS, or a timeout. | Check `endpoint`, the network egress and the firewall. |
| **ERRORING** | The backend is reachable, but it answers with 5xx errors. | The provider is degraded. Wait, or check the status page of the provider. The proxy does **not** block requests in this state, because the backend still serves some of them. Only **CREDENTIALS REJECTED** and **UNREACHABLE** block requests. |

When the badge is not **Connected**, the card also shows an alert with the cause: **Backend unavailable** for the two states that block requests, and **Backend degraded** for **ERRORING**. The same cause string appears in the ERROR line of the boot log, in the body of the `503` that clients receive, and in a rejected config apply, so these places always agree.

## Force a probe now

Click **Test connection** on the backend card. The server runs the probe with its own credentials, endpoint and network, and the card shows the result below the buttons. A green result means that the proxy itself can serve from this backend, which proves more than a browser that can reach the endpoint. The same probe is `POST /_/api/admin/backends/<name>/probe`.

![The hetzner-fsn1 backend card after a probe; one box marks its Connected badge, the arrow points at its Test connection button, and a second box marks the probe result.](/_/screenshots/backend-health-test-connection.webp)

Buckets that an unhealthy verdict blocks reopen by themselves within about 30 seconds of the recovery, and **Test connection** reopens them at once.

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

## Verify

1. Check the live state of every backend. `GET /_/ready` needs no authentication, and its `backends` field maps each backend name to `healthy`, `unreachable`, `auth-rejected` or `erroring`:

   ```bash
   curl -s https://s3.acme.example/_/ready
   # {"status":"ready","backend":"ready","config_db":"ready","backends":{"aws-dr":"healthy","hetzner-fsn1":"healthy","local-disk":"healthy"}}
   ```

2. Check that a bucket on the repaired backend serves again. A list request must succeed instead of answering `503`:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases/
   ```

3. In the proxy log, check for the recovery line of the backend after the next probe.

## Related

- [Backend capability validation](backend-capability-validation.md): the other backend check, for conditional-write (CAS) support in multi-instance deployments.
- [Route a bucket to a backend](route-a-bucket-to-a-backend.md)
- [Configuration reference](../reference/configuration.md): `DGP_BOOT_BACKEND_PROBE`.
- [Admin API reference](../reference/admin-api.md): the probe endpoint and `GET /_/ready`.
