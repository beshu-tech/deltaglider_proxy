# How to view live logs in the admin GUI

This guide shows you how to tail and filter the proxy's operational logs from the admin UI, without SSH and without `grep` on stdout. It also shows how to raise the log level, so that the view shows the debug lines of a request that you want to follow.

The view requires an admin session, so you must be signed in to the admin GUI.

## 1. Open the log view and follow it

In the admin UI:

1. In the sidebar, open **Observability → System logs** (`/_/admin/diagnostics/logs`). The view shows the proxy's operational log stream: security, rate-limit, S3-error, replication and lifecycle lines, captured at `INFO` and above.
2. Narrow the lines with the three filters. The server applies them to the backlog and to the live tail:
   - The level list: **All levels**, **Error**, **Warn+**, **Info+** or **Debug+**. Lines below the capture floor of the ring never enter it (see step 3 below).
   - The target field: a substring match on the log target (the Rust module), for example `auth` or `replication`.
   - The search field: free text over the message and the structured fields, for example a bucket name or a client IP address.
3. Turn on **Follow** to stream new log lines as they happen, over server-sent events. Leave it off to inspect a static snapshot of the recent backlog, and click **Refresh** to load it again.

   ![The System logs page shows a filtered log line; callout 2 marks the level, target and search filters, and callout 3 marks the Follow switch.](/_/screenshots/logs-follow.webp)

Click a row to expand its structured fields.

## 2. Raise the log level to debug

The ring holds only the lines that the log level of the proxy lets through. The default level is `deltaglider_proxy=info,tower_http=info`, so the view shows no debug lines until you raise the level. The log level applies without a restart.

In the admin UI:

1. In the sidebar, open **System → System** (`/_/admin/system`), and scroll to the **Log level** card.
2. In **Level**, select **Debug**. This preset sets the filter `deltaglider_proxy=debug,tower_http=debug`. To type your own filter, select **Custom** and fill in **Custom EnvFilter**.
3. Click **Review & apply** in the bar above the card.

   ![The Log level card of the System page has the Debug preset selected; callout 1 marks Debug and callout 2 marks Review & apply.](/_/screenshots/logs-level-debug.webp)

4. Check the diff in the dialog, and then click **Apply and Persist**.

   ![The review dialog shows that log_level changes to the debug filter; the arrow points at Apply and Persist.](/_/screenshots/logs-apply.webp)

The new level applies from the next request after the apply.

## 3. Widen the capture floor of the ring

The ring also has its own floor, and the default floor is `info`. To keep the debug lines in the ring, set `DGP_LOG_RING_LEVEL=debug` in the environment of the proxy and restart it. The admin UI and the YAML file cannot set the floor. The floor and the size of the ring exist only as environment variables:

- `DGP_LOG_RING_SIZE` (default `2000`) sets the number of lines that the ring holds.
- `DGP_LOG_RING_LEVEL` (default `info`) sets the lowest severity that the ring captures. The ring sees only the lines that the log level of the proxy lets through, so a ring level below the log level captures nothing more.

## The same change in YAML

The log level steps above write this configuration into the `advanced` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
advanced:
  log_level: "deltaglider_proxy=debug,tower_http=debug"
```

Two environment variables override this field: `RUST_LOG`, and after it `DGP_LOG_LEVEL`. When one of them is set, the admin UI shows the field as read-only with a from env badge that names the variable.

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## Reproduce and watch

To debug a specific request, turn **Follow** on, set the level list and a target or a search term, and then send the request. The matching line appears when the proxy logs it.

## What it is and what it is not

The viewer reads a bounded in-memory ring on each instance, and the ring is empty again after every restart. It is a triage tool, not a log store. For retention, search and aggregation across instances, point a log shipper at the stdout of the proxy. Set `DGP_LOG_FORMAT=json` for one JSON object per line, which `jq` can filter and which Loki, Quickwit or an ELK stack can ingest.

## Verify

1. Check that the running proxy uses the debug level:

   ```bash
   curl -b cookies "https://s3.acme.example/_/api/admin/config/section/advanced?format=yaml" | grep log_level
   ```

   The output shows `log_level: deltaglider_proxy=debug,tower_http=debug`.

2. With **Follow** on and **Debug+** selected in the level list, send one request, and check that its debug lines appear in the view within a few seconds:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases/
   ```

   If no debug line appears, check that `DGP_LOG_RING_LEVEL=debug` is set, because the default floor of the ring drops debug lines.

3. Read the same lines over the admin API:

   ```bash
   curl -b cookies "https://s3.acme.example/_/api/admin/logs?level=debug&limit=20"
   ```

## Related

- [Trace and audit requests](trace-requests.md): the audit ring (security events) and the admission-chain tracer.
- [Configuration reference](../reference/configuration.md#structured-logs-and-the-in-gui-log-ring): the logging environment variables.
- [Admin API reference](../reference/admin-api.md): `GET /_/api/admin/logs` and `/_/api/admin/logs/stream`.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
