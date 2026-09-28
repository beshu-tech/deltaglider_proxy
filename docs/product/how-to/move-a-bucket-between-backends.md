# How to move a bucket to another backend

This guide shows you how to relocate the data of a bucket from one backend to another with the built-in migrate job. The job copies every object to the target backend, verifies the copies, and only then switches the route of the bucket. The example moves `db-archive` from `local-disk` to `hetzner-fsn1`. For how the job survives crashes and gates writes, see [jobs and durability](../explanation/jobs-and-durability.md).

Only the admin UI and the admin API can move data. A change of `storage.buckets.<bucket>.backend` in YAML only changes the route, and it copies nothing: the proxy then looks for the objects on the new backend, and the objects on the old backend become invisible ([what routing does not do](route-a-bucket-to-a-backend.md#what-routing-does-not-do)). So this page has no YAML recap.

## Before you start

- Reads keep working for the whole migration.
- **Writes get `503 SlowDown`** while the job runs. AWS SDKs back off and retry automatically, so well-behaved clients only slow down. Pause any client that treats a 503 as fatal. The gate lifts at the moment when the bucket switches to the new backend, before any optional cleanup of the source.
- **The job runs on a single-instance deployment only.** The switch changes the configuration of the instance that runs the job, and config sync does not carry bucket routing to the other instances. The other instances would therefore keep writing to the source, and the cleanup would delete those writes. For this reason the proxy refuses to start a migration with `409 Conflict` while `config_sync_bucket` is set. To move a bucket on a multi-instance deployment, scale down to one instance, and remove `config_sync_bucket` for the duration of the move.
- The target backend must already exist on **Storage → Backends** (see [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md)). The **Migrate data…** link shows only when the proxy has more than one backend.
- The copy goes through the engine, so encryption and delta compression stay transparent: each side applies its own configuration. You can move between backends with different encryption modes or keys.
- **The destination bucket must be empty.** A destination that already holds objects is usually the safety copy of an earlier move. The job would copy on top of that copy, so every object that was deleted since the earlier move would come back. For this reason the job refuses to start on a non-empty destination: it fails in its `stage` phase, before it copies anything, and its error names the number of objects that it found. You then have two choices. You can empty the destination bucket, or you can make the destination an exact mirror of the source (see section 1).

## 1. Start the job

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click the `db-archive` row to open it.
3. Click **Migrate data…** next to **Backend**.

   ![The Buckets page with the db-archive row open; callout 1 marks Buckets in the sidebar, callout 2 marks the db-archive row, and callout 3 marks the Migrate data… link next to Backend.](/_/screenshots/migrate-bucket-link.webp)

4. In the dialog **Migrate db-archive to another backend**, select `hetzner-fsn1` in **Target backend**.
5. Leave **Delete source objects after the switch-over** off. The source copy then stays in place, so that you can remove it after you verify the move (section 4).
6. Click **Start migration**.

   ![The dialog Migrate db-archive to another backend; callout 4 marks Target backend, set to hetzner-fsn1, callout 5 marks the Delete source objects after the switch-over checkbox, which stays off, and callout 6 marks Start migration.](/_/screenshots/migrate-modal.webp)

You can also start the job on **Storage → Jobs** with **New job** → **Migrate bucket… — one-off move**. That dialog has a **Bucket** list in addition.

To move the bucket back onto a backend that still holds the old safety copy, select **Make the destination an exact mirror of the source** in the dialog. In mirror mode, the job makes the destination an exact copy of the source: after the verify phase and before the switch, it deletes every destination object that the source does not hold. The job writes each of these deletes to the audit log as `maintenance_migrate_mirror_delete`.

The same job with the admin API:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/buckets/db-archive/migrate \
  -H 'Content-Type: application/json' \
  -d '{"target_backend": "hetzner-fsn1", "delete_source": false}'
```

The response is `202 Accepted` with a `maintenance:<n>` job id. `delete_source` defaults to `false`. For mirror mode, add `"target": "mirror"`:

```bash
curl -b cookies -X POST \
  https://s3.acme.example/_/api/admin/buckets/db-archive/migrate \
  -H 'Content-Type: application/json' \
  -d '{"target_backend": "local-disk", "target": "mirror"}'
```

The job stages the destination, copies every object through the engine, verifies the copies, switches the route of the bucket to the new backend, and cleans up.

Each copy keeps the original creation time of the object, which S3 clients see as `LastModified`. So after the move, a listing shows the same dates as before, lifecycle rules count the age of an object from its original upload, and a newer-wins comparison sees the real times.

## 2. Watch it run

In the sidebar, open **Storage → Jobs** (`/_/admin/jobs`). The migration is a row of the kind **Migrate**, with its status and live progress. Click the row to open its drawer, which shows the phase, the progress in objects and bytes, and any failures per object.

![The Jobs page lists the running migrate job of db-archive to hetzner-fsn1; the box marks the job row.](/_/screenshots/migrate-jobs-row.webp)

The admin API returns the same data:

```bash
curl -b cookies https://s3.acme.example/_/api/admin/jobs
curl -b cookies https://s3.acme.example/_/api/admin/jobs/maintenance:7/failures
```

If something looks wrong, click **Cancel** in the row of the job (or send `POST /_/api/admin/jobs/maintenance:7/cancel`). The job checks for a cancel every 20 objects. A cancel before the switch unwinds cleanly: writes to the source bucket resume at once, the job deletes the copies that it wrote to the destination, and the job never deletes the source on a failed or cancelled run.

A restart of the proxy in the middle of the job does not leave the bucket behind: the proxy puts the job back in the queue at startup, and the job resumes from its cursor ([details](../explanation/jobs-and-durability.md)).

## 3. Verify

1. Check that the row of the job on **Storage → Jobs** shows `succeeded`, and that the `db-archive` row on **Storage → Buckets** now shows the backend `hetzner-fsn1`.
2. Read an object through the proxy, and compare it to a checksum from before the migration:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp s3://db-archive/nightly/2026-06-10.dump - | sha256sum
   ```

3. Check that writes work again:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp probe.txt s3://db-archive/probe.txt
   ```

4. Check that the object counts match. List the bucket through the proxy, and compare the result with the listing of the source backend itself.

## 4. Clean up the source (optional)

After you verify the move, delete the old copy yourself. In the example, the old copy is the `db-archive` data directory on `local-disk`. If you want the job to delete it, select **Delete source objects after the switch-over** when you start the migration (`"delete_source": true` in the API). The cleanup then runs only after the switch succeeds. A cleanup that cannot finish never fails the migration, because the bucket already lives on the new backend. This happens when a source delete fails, when you cancel the job during the cleanup, or when the bucket no longer routes to the target. The job then settles as `completed_with_errors` with a note, and you remove the remaining source objects yourself.

## Related

- [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md): declare the target backend first.
- [How to rotate or change encryption keys](rotate-encryption-keys.md): a migration is also the key-rotation path that needs no legacy key.
- [Jobs reference](../reference/jobs.md): the unified jobs API, the write gate, and the capability matrix.
- [Jobs and durability](../explanation/jobs-and-durability.md): crash-resume, leases, and the write gate.
