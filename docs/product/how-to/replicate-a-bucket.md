# How to replicate a bucket to another backend

This guide shows you how to mirror a bucket to a second backend with a replication rule. A rule copies every new object of the source bucket to the destination bucket, and a periodic reconcile repairs anything that the copies missed. The example mirrors `releases` to `releases-dr` on the `aws-dr` backend with the rule `releases-to-dr`. For the full rule grammar and the failure semantics, see the [replication reference](../reference/replication.md).

## 1. Prepare the destination bucket

The destination bucket must exist, and it must route to the backend that holds the copies. In the example, `releases-dr` routes to `aws-dr`. [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md) shows how to set the route in the admin UI and in YAML.

## 2. Create the rule

In the admin UI:

1. In the sidebar, open **Storage → Jobs** (`/_/admin/jobs`).
2. Click **New job**.
3. Click **Replication rule — continuous copy**. A drawer opens with the definition of a new rule.

   ![The Jobs page with the New job menu open; callout 1 marks Jobs in the sidebar, callout 2 marks New job, and callout 3 marks Replication rule — continuous copy.](/_/screenshots/replicate-new-job.webp)

4. In **Rule name**, type `releases-to-dr`.
5. Leave **Enabled** on.
6. In **Source**, type `releases` as the bucket, and leave the prefix empty. An empty prefix copies the entire bucket.
7. In **Destination**, type `releases-dr` as the bucket, and leave the prefix empty. The copies then keep the key paths of the source.

   ![The Definition tab of the releases-to-dr replication rule; callout 4 marks Rule name, callout 5 marks Enabled, callout 6 marks Source, set to releases, and callout 7 marks Destination, set to releases-dr.](/_/screenshots/replicate-rule-fields.webp)

8. Click **Advanced rule behavior**. In **Interval**, type `1h`. Leave **Exclude globs** at `.deltaglider/**`. Leave **Conflict policy** at **Newer wins — safest default**, and leave **Delete replication** off. Sections 3 to 5 explain these choices.

   ![The Advanced rule behavior part of the releases-to-dr rule is open; the box marks the Conflict policy choices, and the arrow points at the Delete replication switch.](/_/screenshots/replicate-rule-advanced.webp)

9. Click **Review & apply** in the bar at the bottom of the page. The dialog lists each changed field and a **Replication plan** with every rule. Check it, and then click **Apply and Persist**.

   ![The review dialog lists the changed fields of the releases-to-dr rule and the Replication plan; the arrow points at Apply and Persist.](/_/screenshots/replicate-apply.webp)

Replication has two triggers. The event-driven trigger copies each PUT, DELETE and COPY in near real time, and it is the primary path. The **Interval** of the rule schedules a periodic full reconcile. The reconcile is the backstop that repairs anything that the event path missed. Both triggers always run, and the interval only sets how often the backstop sweeps ([details](../reference/replication.md#triggers)).

Every copy goes through the engine, so each side applies its own encryption and compression. You can therefore replicate from an encrypted backend to a plaintext one, and the other way around.

## 3. Scope what replicates

- If only part of the bucket matters, give **Source** a prefix, for example `firmware/`. To narrow the scope further, add a pattern to **Include globs**, for example `firmware/widget-3000/**`. When include globs are set, only the matching keys replicate.
- If some keys must never leave the source (scratch files, temporary uploads), add them to **Exclude globs**. An exclude wins over an include. Keep `.deltaglider/**` in the list, because it protects the config-sync prefix when the bucket also holds user data. A new rule that you create in the admin UI starts with `.deltaglider/**` in this field.
- If the destination must use a different layout, give **Destination** a prefix. The rule then places the source keys under that prefix.

Directory markers and storage-layer delta artifacts never replicate, because the engine listing filters them out before the planning ([full list](../reference/replication.md#what-doesnt-replicate)).

## 4. Pick a conflict policy

- If nothing else writes to the destination (a write-only disaster-recovery copy), keep **Newer wins — safest default** (`newer-wins`). A copy then happens only when the source object is strictly newer.
- If the destination must stay an exact mirror of the source, even over manual edits on the destination, select **Content diff — mirror (copy only when bytes differ)** (`content-diff`). It overwrites any object whose bytes differ, and it skips identical objects. So it converges, instead of copying everything again on every sweep.
- If you seed a bucket once and never overwrite it, select **Skip existing destination objects** (`skip-if-dest-exists`).

## 5. Decide on delete replication

By default, deletes do not propagate, so `releases-dr` keeps the objects that disappear from `releases`. If you want a true mirror, turn on **Delete replication** (`replicate_deletes: true`). The rule then removes **any** destination object that the source does not hold. It deletes a destination object only after a HEAD request to the source confirms that the key is gone. Because it removes everything that is not at the source, including objects that other tools or another rule wrote, the destination bucket (`releases-dr`) must be **dedicated to this rule**.

## The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
    - name: aws-dr
      type: s3
      region: eu-west-1
  buckets:
    releases-dr:
      backend: aws-dr
  replication:
    enabled: true
    rules:
      - name: releases-to-dr
        enabled: true
        source:
          bucket: releases
          prefix: ""              # "" = the entire bucket
        destination:
          bucket: releases-dr
          prefix: ""
        interval: "1h"            # the full-reconcile safety net
        batch_size: 100
        replicate_deletes: false
        conflict: newer-wins
        exclude_globs: [".deltaglider/**"]
```

`storage.replication.enabled` is the global switch of replication, and it is `true` by default. The admin UI has no control for it. When it is `false`, no rule runs, and **Run now** is refused.

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## 6. Run it now

The first sync does not have to wait for events or for the interval. In the admin UI:

1. On **Storage → Jobs**, click **Run now** in the row of `releases-to-dr`. The wide table shows the actions as icons, and each icon shows its name when you point at it.

   ![The Jobs page lists the releases-to-dr replication rule; the arrow points at its Run now button.](/_/screenshots/replicate-run-now.webp)

A rule that is disabled or paused shows **Run once** instead. It runs the rule one time, and it does not enable or resume the rule.

With the admin API:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/jobs/replication:releases-to-dr/run-now
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

The run opens its row in the run history after this response, so `run_id` is `0` and the counters are empty. To see the result, read the runs of the rule (section 7) until the newest run has a terminal status.

If you get `409 Conflict`, the rule is already running, replication is disabled globally (`storage.replication.enabled: false`), or a maintenance job is active on `releases-dr`. The error message says which one.

## 7. Watch it in Jobs

On **Storage → Jobs**, the rule is the row `releases-to-dr`, with its status and its last run. Click the row to open its drawer. The **Runs** tab lists every execution, and the **Failures** tab lists the errors per object. A few failed objects do not fail the run, because the next pass copies them.

![The Runs tab of the replication job shows one finished run that copied five objects; the arrow points at the number of copied objects.](/_/screenshots/job-runs.webp)

The same data with the admin API:

```bash
curl -b cookies https://s3.acme.example/_/api/admin/jobs/replication:releases-to-dr/runs?limit=10
curl -b cookies https://s3.acme.example/_/api/admin/jobs/replication:releases-to-dr/failures
```

To stop the rule for a while, click **Pause** in its row (or send `POST …/pause`), and click **Resume** to start it again (`POST …/resume`). A paused rule copies and deletes nothing, also for new events, and the pause survives restarts. The proxy does not keep the events of the pause, so after a resume the next scheduler tick starts a full reconcile to bring `releases-dr` in sync. With delete replication on, that reconcile also applies the deletes of the pause.

## Verify

1. Open the drawer of `releases-to-dr`, click the **Verify** tab, and click **Run audit**. The audit is a fast metadata check: it lists both sides, and it checks that every source object exists on the destination with matching recorded checksums and sizes. It downloads nothing, so it does not read the bytes on the destination again. It returns one verdict: **Verified in sync**, **Not fully verified** (the scan was capped, or some objects matched only by size), or **Differences found**, with a guided fix for each finding.

   ![The Verify tab of the releases-to-dr rule explains the metadata audit; the arrow points at the Run audit button.](/_/screenshots/replicate-verify.webp)

   The audit proves that the two sides agree on the recorded metadata. It does not prove that the destination reconstructs each object byte for byte. For that proof, use the checks below.

2. Check that the destination has the objects:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases-dr/firmware/widget-3000/
   ```

3. Check that the content is byte-identical:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://releases-dr/firmware/widget-3000/fw-2.4.1.tar - | sha256sum
   ```

4. Check that the near-real-time copy works. Upload a new object to `releases`, and watch it appear on `releases-dr` within seconds:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp fw-2.4.2.tar s3://releases/firmware/widget-3000/fw-2.4.2.tar
   aws --endpoint-url https://s3.acme.example s3 ls s3://releases-dr/firmware/widget-3000/
   ```

5. Check that the run history shows `succeeded` with `errors: 0`.

## Related

- [How to use a backend without conditional writes](backend-capability-validation.md): mark the destination `replication_target_only` to host a mirror on a cheap backend like Backblaze B2.
- [Replication reference](../reference/replication.md): rule grammar, conflict policies, failure modes, and what does not replicate.
- [Jobs reference](../reference/jobs.md): the unified jobs API that shows the rule.
- [Event log reference](../reference/event-outbox.md): the event stream that drives the near-real-time copies.
- [Jobs and durability](../explanation/jobs-and-durability.md): why replication is a durable job.
- [How to expire and archive objects](expire-and-archive-objects.md): age-based moves instead of mirroring.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
