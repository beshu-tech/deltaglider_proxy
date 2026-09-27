# How to expire and archive objects

This guide shows you how to delete objects past a certain age with a lifecycle rule, or transition them to a colder bucket instead. It also shows how to preview the blast radius before the rule deletes anything. The full rule grammar is in the [lifecycle reference](../reference/lifecycle.md).

## 1. Write an expiry rule

Delete nightly DB dumps older than 90 days from `db-archive`:

```yaml
# validate
storage:
  lifecycle:
    enabled: true                  # global switch — required for execution
    rules:
      - name: expire-nightly-dumps
        enabled: false             # keep off until you've previewed
        bucket: db-archive
        prefix: "nightly/"
        action: delete
        expire_after: "90d"
        include_globs: ["nightly/**/*.dump"]
        exclude_globs: ["nightly/golden/**"]
```

In the admin UI, open **Settings → Jobs**. Lifecycle rules live in the storage-section editor on the Jobs screen.

Lifecycle is off by default, and three conditions must be true: the rule must exist, `lifecycle.enabled` must be `true`, and the rule's own `enabled` must be `true` before the scheduler or run-now deletes anything. Leave the rule disabled until step 3.

## 2. Or write a transition rule

If the objects should move somewhere cold instead of being deleted, make `action` a transition:

```yaml
        action:
          type: transition
          destination:
            bucket: db-archive
            prefix: "cold/nightly/"
          delete_source_after_success: false   # copy (archive) semantics
```

`delete_source_after_success: false` archives the objects, and the source stays. Set it to `true` for move semantics. Lifecycle then copies first, verifies the destination, and deletes the source only after the copy succeeds.

## 3. Preview first

Always do a dry run before you enable the rule. Preview works even while the rule is disabled. It is strictly read-only, and it writes no history rows:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/preview
```

In the admin UI, press the **Preview** button on the rule's row on the Jobs screen. It opens the rule's drawer on the **Preview** tab, which lists the candidate keys with the total count and bytes. The list stays there until you close the drawer, and **Refresh preview** computes it again.

![Lifecycle preview](/_/screenshots/lifecycle-preview.jpg)

Check the response: `objects_affected` and `bytes_affected` are the blast radius, and `candidates` lists exactly which keys would be deleted or transitioned, with their age and size. If the candidate list contains anything that should survive, fix the rule's `prefix`, globs or `expire_after`, and preview again until the list is right.

## 4. Enable and run

Set the rule's `enabled: true` and apply. The scheduler now runs the rule when it is due. To run it immediately instead of waiting:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/run-now
```

The request does not wait for the run. It answers `202 Accepted` with the `run_id` of the new run and `status: "running"`, and the run continues in the background. Step 5 shows how to read its result. A `409` means that lifecycle or the rule is disabled, the rule is paused or already running, or a maintenance job is active on a bucket that the rule writes to.

In the admin UI, **Run now** on a lifecycle rule does not start the run at once. The proxy first computes a preview, and a dialog shows the objects that the run would delete or move, with their count and total size. The run starts only when you press the button that names the count, for example **Run: delete 12 objects**. The run acts on the objects that match the rule when it starts, so the result can differ slightly from the preview.

If you need to stop the rule temporarily (incident, audit freeze), pause it from the job row or with `POST …/pause`. Both the scheduler and run-now skip a paused rule, and the pause survives restarts. `…/resume` turns the rule back on.

## 5. Read the history

The proxy persists every execution. On the Jobs screen, the rule's drawer shows **Runs** (when, triggered by scheduler or run-now, objects/bytes affected, terminal status) and **Failures** (per-object errors with the run that observed them). Via the API:

```bash
curl -b cookies https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/runs?limit=10
curl -b cookies https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/failures
```

## The guardrails that protect you

Lifecycle never touches:

- Directory markers (`folder/`).
- DeltaGlider internal prefixes (`.deltaglider/**`, `.dg/**`) and storage artifacts (`reference.bin`, `*.delta`).
- Keys matched by `exclude_globs`, or outside `include_globs` when includes are set.
- Keys newer than `expire_after`.

The design also protects you in these ways:

- A failed transition copy never deletes the source.
- Deletes are idempotent.
- Preview takes no locks and writes nothing.
- A run that a crash interrupts resumes from a stored cursor instead of rescanning ([how](../explanation/jobs-and-durability.md)).

## Verify

1. After the first run, the run history shows `succeeded` and `objects_affected` matches what the preview predicted.
2. An expired key is gone (or landed in the cold prefix for transitions):

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://db-archive/nightly/
   aws --endpoint-url https://s3.acme.example s3 ls s3://db-archive/cold/nightly/
   ```

3. Excluded keys (`nightly/golden/**`) are still there.
4. If you have event delivery configured, `LifecycleExpired` / `LifecycleTransitioned` events appear in the event log at **Settings → Integrations → Event log** ([how to send them somewhere](send-event-notifications.md)).

## Related

- [Lifecycle reference](../reference/lifecycle.md): full grammar, run/failure schemas, guardrail list.
- [Jobs reference](../reference/jobs.md): the unified jobs surface and capability matrix.
- [How to replicate a bucket to another backend](replicate-a-bucket.md): continuous mirroring instead of age-based moves.
- [Jobs and durability](../explanation/jobs-and-durability.md): leases, crash-resume, and why preview is safe.
