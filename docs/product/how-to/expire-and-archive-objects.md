# How to expire and archive objects

This guide shows you how to delete objects past a certain age with a lifecycle rule, or how to move them to a colder place instead. It also shows how to preview the objects that a rule would touch before the rule deletes anything. A lifecycle rule runs only when three switches are on: the rule exists, its own **Enabled** switch is on, and the global switch **Run lifecycle rules on schedule** is on. The full rule grammar is in the [lifecycle reference](../reference/lifecycle.md).

The example deletes the nightly database dumps in `db-archive` that are older than 90 days, with the rule `expire-nightly-dumps`.

## 1. Create the rule, and keep it disabled

In the admin UI:

1. In the sidebar, open **Storage → Jobs** (`/_/admin/jobs`), and click **New job**.
2. Click **Lifecycle rule — scheduled expiry / archive**. A drawer opens with the definition of a new rule.

   ![The Jobs page with the New job menu open; callout 1 marks New job and callout 2 marks Lifecycle rule — scheduled expiry / archive.](/_/screenshots/lifecycle-new-job.webp)

3. In **Rule name**, type `expire-nightly-dumps`.
4. Leave **Enabled** off. A new lifecycle rule starts disabled, so that you can preview it first (section 3).
5. In **Scope**, type `db-archive` as the bucket and `nightly/` as the prefix.
6. In **Expire after**, type `90d`. The rule then selects the objects that were created more than 90 days ago.
7. Leave **Action** at **Delete**.

   ![A new lifecycle rule named expire-nightly-dumps; callout 3 marks Rule name, callout 4 marks the Enabled switch, which stays off, callout 5 marks Scope, set to db-archive and nightly/, callout 6 marks Expire after, set to 90d, and callout 7 marks Action, set to Delete.](/_/screenshots/lifecycle-rule-fields.webp)

8. Click **Filters and batch size**. In **Include globs**, type `nightly/**/*.dump`. In **Exclude globs**, keep `.deltaglider/**`, and add `nightly/golden/**` on a new line.

   ![The Filters and batch size part of the expire-nightly-dumps rule is open; the box marks Include globs and Exclude globs, which hold nightly/**/*.dump and nightly/golden/**.](/_/screenshots/lifecycle-rule-filters.webp)

9. Click **Review & apply** in the bar at the bottom of the page. Check the new rule in the dialog, and then click **Apply and Persist**.

   ![The review dialog lists the new expire-nightly-dumps lifecycle rule; the arrow points at Apply and Persist.](/_/screenshots/lifecycle-apply.webp)

The exclude globs replace the default list, which holds only `.deltaglider/**`. Keep `.deltaglider/**` in the list, because it protects the config-sync prefix.

## 2. Or archive instead of delete

If the objects must move somewhere cold instead of disappearing, change the action of the rule. In the admin UI:

1. In the definition of the rule, select **Archive / move** in **Action**.
2. In **Destination**, type `db-archive` as the bucket and `cold/nightly/` as the prefix.
3. Leave **Delete source after copy** off.

   ![The expire-nightly-dumps rule with Action set to Archive / move; callout 1 marks Action, callout 2 marks Destination, set to db-archive and cold/nightly/, and callout 3 marks Delete source after copy, which stays off.](/_/screenshots/lifecycle-transition.webp)

4. Click **Review & apply**, and then click **Apply and Persist**.

With **Delete source after copy** off, the rule archives: it copies the objects, and the source objects stay. Turn it on for move semantics. The rule then copies first, verifies the destination, and deletes a source object only after its copy succeeds. The copy keeps the creation time of the source object, so the archived object shows its original `LastModified`.

## The same change in YAML

Sections 1 and 4 write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). The block shows the final state, after you turn on the two switches of section 4:

```yaml
# validate
storage:
  lifecycle:
    enabled: true                  # "Run lifecycle rules on schedule" (default: false)
    rules:
      - name: expire-nightly-dumps
        enabled: true              # keep false until you preview the rule
        bucket: db-archive
        prefix: "nightly/"
        action: delete
        expire_after: "90d"
        include_globs: ["nightly/**/*.dump"]
        exclude_globs: [".deltaglider/**", "nightly/golden/**"]
        batch_size: 100
```

For the archive of section 2, `action` is a tagged object instead of the string `delete` (**Archive / move** in the admin UI is `type: transition`):

```yaml
# validate
storage:
  lifecycle:
    enabled: true
    rules:
      - name: expire-nightly-dumps
        enabled: true
        bucket: db-archive
        prefix: "nightly/"
        action:
          type: transition
          destination:
            bucket: db-archive
            prefix: "cold/nightly/"
          delete_source_after_success: false   # false = archive (copy), true = move
        expire_after: "90d"
        include_globs: ["nightly/**/*.dump"]
        exclude_globs: [".deltaglider/**", "nightly/golden/**"]
```

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## 3. Preview first

Always preview a rule before you enable it. The preview lists the objects that a run would delete or move now. It works while the rule is disabled, it is strictly read-only, and it writes no history rows. In the admin UI:

1. On **Storage → Jobs**, click **Preview** in the row of `expire-nightly-dumps`. The drawer of the rule opens on the **Preview** tab, which lists the candidate keys with their total count and size.
2. Check the list. If it contains anything that must survive, change the **Scope**, the globs or **Expire after** of the rule, apply the change, and click **Refresh preview**. Repeat until the list is right.

   ![The Preview tab of the expire-nightly-dumps rule lists three dumps that the rule would delete, with their total size; the arrow points at Refresh preview.](/_/screenshots/lifecycle-preview.webp)

The list stays on the tab until you close the drawer. The same preview with the admin API:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/preview
```

In the response, `objects_affected` and `bytes_affected` give the size of the change, and `candidates` lists exactly the keys that a run would delete or move, with their age and size.

## 4. Enable and run

In the admin UI:

1. Click the row of `expire-nightly-dumps`, and turn on **Enabled** on the **Definition** tab.
2. On the Jobs page, turn on **Run lifecycle rules on schedule**. It is off by default, and while it is off, no lifecycle rule runs.

   ![The Jobs page with the switch Run lifecycle rules on schedule, which is on; the arrow points at the switch.](/_/screenshots/lifecycle-scheduler-switch.webp)

3. Click **Review & apply**, and then click **Apply and Persist**. The scheduler now runs the rule when it is due. It checks for due rules every hour (`storage.lifecycle.tick_interval`).
4. To run the rule at once, click **Run now** in its row. The run does not start yet: the proxy first computes a preview, and a dialog shows the objects that the run would delete or move, with their count and total size.
5. Click the button that names the count, for example **Run: delete 3 objects**. The run acts on the objects that match the rule when it starts, so the result can differ slightly from the preview.

   ![The dialog Run lifecycle rule "expire-nightly-dumps" now? lists the three dumps that the run would delete; the arrow points at the button Run: delete 3 objects.](/_/screenshots/lifecycle-run-confirm.webp)

The row shows **Run now** disabled while the rule cannot run: when the rule is disabled or paused, or when **Run lifecycle rules on schedule** is off. The title of the button says what to turn on first. A lifecycle run deletes or moves objects, so unlike replication there is no one-time run of a disabled rule.

With the admin API, run-now does not ask for a confirmation:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/run-now
```

The request does not wait for the run. It answers `202 Accepted` with the `run_id` of the new run and `status: "running"`, and the run continues in the background. Section 5 shows how to read its result. A `409 Conflict` means that lifecycle or the rule is disabled, that the rule is paused or already running, or that a maintenance job is active on a bucket that the rule writes to. The error message says which one.

To stop the rule for a while (an incident, an audit freeze), click **Pause** in its row, or send `POST …/pause`. The scheduler and run-now both skip a paused rule, and the pause survives restarts. **Resume** (`POST …/resume`) turns the rule back on.

## 5. Read the history

The proxy keeps a record of every execution. On the Jobs page, the drawer of the rule has two tabs for it. **Runs** lists each run: when it started, whether the scheduler or run-now started it, the objects and bytes it affected, and its final status. **Failures** lists the errors per object, with the run that saw them. With the admin API:

```bash
curl -b cookies https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/runs?limit=10
curl -b cookies https://s3.acme.example/_/api/admin/jobs/lifecycle:expire-nightly-dumps/failures
```

## The guardrails that protect you

Lifecycle never touches:

- Directory markers (`folder/`).
- DeltaGlider internal prefixes (`.deltaglider/**`, `.dg/**`) and storage artifacts (`reference.bin`, `*.delta`).
- Keys that match `exclude_globs`, or keys outside `include_globs` when include globs are set.
- Keys newer than `expire_after`.

The design also protects you in these ways:

- A failed transition copy never deletes the source.
- Deletes are idempotent.
- A preview takes no locks and writes nothing.
- A run that a crash interrupts resumes from a stored cursor instead of scanning again from the start ([how](../explanation/jobs-and-durability.md)).

## Verify

1. After the first run, check that the **Runs** tab shows `succeeded`, and that the number of affected objects matches the preview.
2. Check that an expired key is gone, or that it is in the cold prefix after an archive:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 ls s3://db-archive/nightly/
   aws --endpoint-url https://s3.acme.example s3 ls s3://db-archive/cold/nightly/
   ```

3. Check that the excluded keys (`nightly/golden/**`) are still there.
4. If you configured event delivery, check that the `LifecycleExpired` or `LifecycleTransitioned` events appear on **Integrations → Event log** (`/_/admin/integrations/event-outbox`) ([how to send them somewhere](send-event-notifications.md)).

## Related

- [Lifecycle reference](../reference/lifecycle.md): the full grammar, the run and failure schemas, and the guardrail list.
- [Jobs reference](../reference/jobs.md): the unified jobs surface and its capability matrix.
- [How to replicate a bucket to another backend](replicate-a-bucket.md): continuous mirroring instead of age-based moves.
- [Jobs and durability](../explanation/jobs-and-durability.md): leases, crash-resume, and why a preview is safe.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
