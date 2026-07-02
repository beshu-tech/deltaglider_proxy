# How to view live logs in the admin GUI

This guide shows you how to tail and filter the proxy's operational logs from the admin UI — no SSH, no `grep` on stdout.

## Where it lives

Open **Settings → Observability → System logs** (`/_/admin/diagnostics/logs`). The view shows the proxy's operational log stream — security, rate-limit, S3-error, replication, and lifecycle lines — captured at `INFO` and above.

This is admin-session-gated: you must be signed in to the admin GUI.

## Tail logs live

Toggle **Follow** to stream new log lines as they happen (over server-sent events). Leave it off to inspect a static snapshot of the recent backlog and refresh on demand.

## Filter

Three filters narrow both the backlog and the live tail, server-side:

- **Level** — `Error`, `Warn+`, `Info+`, `Debug+`. (Lines below the capture floor never enter the ring; see below.)
- **Target** — substring match on the log target (Rust module), e.g. `auth` or `replication`.
- **Search** — free-text over the message and structured fields, e.g. a bucket name or client IP.

Click a row to expand its structured fields.

## Reproduce-and-watch

To debug a specific request, turn **Follow** on, set the level and a target or search term, then trigger the request. The matching line appears as it's logged. For per-request trace detail, widen the capture floor with `DGP_LOG_RING_LEVEL=debug` (see below) and restart.

## What it is — and isn't

The viewer reads an **in-memory, per-instance, bounded ring**. It is a triage convenience, not a log store:

- `DGP_LOG_RING_SIZE` (default `2000`) sets the ring capacity.
- `DGP_LOG_RING_LEVEL` (default `info`) sets the minimum severity captured, independent of `DGP_LOG_LEVEL`.

For retention, search, and aggregation across instances, point a log shipper at the proxy's stdout. Set `DGP_LOG_FORMAT=json` for one JSON object per line, which is `jq`-greppable and ingests cleanly into Loki, Quickwit, or an ELK stack.

## Related

- [Trace and audit requests](trace-requests.md) — the audit ring (security events) and the admission-chain tracer.
- [Configuration reference](../reference/configuration.md#structured-logs-and-the-in-gui-log-ring) — the logging env vars.
- [Admin API reference](../reference/admin-api.md) — `GET /_/api/admin/logs` and `/_/api/admin/logs/stream`.
