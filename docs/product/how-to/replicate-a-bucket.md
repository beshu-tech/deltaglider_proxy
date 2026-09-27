# How to replicate a bucket to another backend

This guide shows you how to mirror a bucket to a second backend with a replication rule. The example mirrors `releases` to `releases-dr` on the `aws-dr` backend. For the full rule grammar and failure semantics, see the [replication reference](../reference/replication.md).

## 1. Create the destination bucket

Route `releases-dr` to the backend that should hold the copies:

```yaml
storage:
  buckets:
    releases-dr:
      backend: aws-dr
```

## 2. Define the rule

Add the rule under `storage.replication`:

```yaml
# validate
storage:
  replication:
    enabled: true
    rules:
      - name: mirror-releases-to-dr
        enabled: true
        source:
          bucket: releases
          prefix: ""              # "" = entire bucket
        destination:
          bucket: releases-dr
          prefix: ""
        interval: "24h"           # full-reconcile safety net
        replicate_deletes: false
        conflict: newer-wins
        exclude_globs: [".deltaglider/**"]
```

In the admin UI, open **Settings → Jobs**. Replication rules live in the storage-section editor on the Jobs screen. Add the rule there and apply it.

![Object replication settings](/_/screenshots/object-replication.jpg)

Replication has two triggers. The event-driven trigger copies each PUT/DELETE/COPY in near-real time, and it is the primary path. The rule's `interval` schedules a periodic full reconcile, which is the backstop that repairs anything missed. Both triggers always run: `interval` only sets how often the backstop sweeps ([details](../reference/replication.md#triggers)).

Every copy goes through the engine, so each side applies its own encryption and compression. You can therefore replicate from an encrypted backend to a plaintext one, and the other way around.

## 3. Scope what replicates

- If only part of the bucket matters, set `source.prefix` (for example `firmware/`). You can narrow the scope further with `include_globs: ["firmware/widget-3000/**"]`. When includes are set, only matching keys replicate.
- If some keys must never leave the source (scratch files, temp uploads), add them to `exclude_globs`. An exclude wins over an include. Keep `.deltaglider/**` excluded, because it protects the config-sync prefix when a bucket also holds user data.
- If the destination should use a different layout, set `destination.prefix`. The rule places the source keys under that prefix.

Directory markers and storage-layer delta artifacts never replicate; the engine listing filters them before planning ([full list](../reference/replication.md#what-doesnt-replicate)).

## 4. Pick a conflict policy

- If the destination is write-only DR (nothing else writes to `releases-dr`), keep the default `newer-wins`: copies happen only when the source is strictly newer.
- If the destination must stay an exact mirror of the source, even over manual edits on the destination, use `content-diff`. It overwrites any object whose bytes differ, and it skips identical ones, so it converges instead of copying everything again on every sweep.
- If you seed a bucket once and never overwrite it, use `skip-if-dest-exists`.

## 5. Decide on delete replication

By default, deletes do not propagate, so `releases-dr` keeps objects that disappear from `releases`. If you want a true mirror, set `replicate_deletes: true`. The destination then becomes a faithful mirror: the rule removes **any** object that is absent at the source. It deletes a destination object only after a source HEAD confirms that the key is gone. The rule removes anything not present at the source, including objects that other tools or another rule wrote. So the destination bucket (`releases-dr`) must be **dedicated to this rule**.

## 6. Run it now

The first sync does not have to wait for events or for the interval:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/jobs/replication:mirror-releases-to-dr/run-now
```

The proxy starts the run in the background and answers `202 Accepted` at once:

```json
{
  "run_id": 0,
  "status": "running",
  "objects_scanned": 0,
  "objects_copied": 0,
  "objects_skipped": 0,
  "bytes_copied": 0,
  "errors": 0
}
```

The run opens its row in the run history after this response, so `run_id` is `0` and the counters are empty. To see the result, poll the runs of the rule (step 7) until the newest run has a terminal status. Run-now also runs a rule that is disabled or paused, once.

If you get `409 Conflict`, the rule is already running, replication is disabled globally (`storage.replication.enabled: false`), or a maintenance job is active on `releases-dr`. Check the Jobs screen.

## 7. Watch it in Jobs

**Settings → Jobs** shows the rule as row `replication:mirror-releases-to-dr` with its status and last run. The drawer's **Runs** tab lists every execution, and **Failures** lists per-object errors. A few failed objects do not fail the run, because the next pass copies them.

![Job runs drawer](/_/screenshots/jobs-drawer-runs.jpg)

The same data via the API:

```bash
curl -b cookies https://s3.acme.example/_/api/admin/jobs/replication:mirror-releases-to-dr/runs?limit=10
curl -b cookies https://s3.acme.example/_/api/admin/jobs/replication:mirror-releases-to-dr/failures
```

Pause and resume from the job row (or `POST …/pause` / `…/resume`). A paused rule copies and deletes nothing, also for new events, and the pause survives restarts. The proxy does not keep the events of the pause, so a resume starts a full reconcile at the next scheduler tick to bring `releases-dr` in sync. With `replicate_deletes: true`, that reconcile also applies the deletes of the pause.

## Verify

The **Audit** button on the rule's Verify tab runs a fast **metadata audit**: it lists both sides and checks that every source object exists on the destination with matching recorded checksums and sizes. It downloads nothing, so it does **not** read the destination's stored bytes again. It proves that the two sides *agree on recorded metadata*, but not that the destination reconstructs each object byte for byte. It returns one verdict, **Verified in sync**, **Not fully verified** (capped scan or size-only matches), or **Differences found**, with a guided fix for each finding. For byte-level proof, use the CLI checks below.

1. The destination has the objects:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases-dr/firmware/widget-3000/
   ```

2. Content is byte-identical:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases-dr/firmware/widget-3000/fw-2.4.1.tar - | sha256sum
   ```

3. Near-real-time copy works. Upload a new object to `releases`, and watch it appear on `releases-dr` within seconds:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp fw-2.4.2.tar s3://releases/firmware/widget-3000/fw-2.4.2.tar
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases-dr/firmware/widget-3000/
   ```

4. The run history shows `succeeded` with `errors: 0`.

## Related

- [How to use non-CAS backends safely](backend-capability-validation.md): mark the destination `replication_target_only` to host a mirror on a cheap backend like Backblaze B2.
- [Replication reference](../reference/replication.md): rule grammar, conflict policies, failure modes, what doesn't replicate.
- [Jobs reference](../reference/jobs.md): the unified jobs API that shows the rule.
- [Event log reference](../reference/event-outbox.md): the event stream that drives near-real-time copies.
- [Jobs and durability](../explanation/jobs-and-durability.md): why replication is a durable job.
- [How to expire and archive objects](expire-and-archive-objects.md): age-based moves instead of mirroring.
