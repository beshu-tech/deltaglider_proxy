# How to set per-bucket compression and quotas

This guide shows you how to turn delta compression off for a bucket, how to make the delta cutoff of a bucket stricter, and how to cap the size of a bucket with a soft quota. All three settings are part of the bucket policy under `storage.buckets`, so you can set them in the admin UI or in the YAML file. For how the proxy decides between a delta and a full copy, see [delta compression](../explanation/delta-compression.md).

The example turns compression off for `downloads`, keeps only good deltas in `releases`, and caps `db-archive` at 500 GiB.

## 1. Turn compression off for a bucket

A bucket that holds content that does not delta (images, video, binaries that are unique on every upload) gains nothing from xdelta3. When compression is off, the proxy stores every object in the bucket as-is and skips the xdelta3 CPU cost.

In the admin UI:

1. In the sidebar, open **Storage → Buckets** (`/_/admin/storage/buckets`).
2. Click the `downloads` row to open it.

   ![The Buckets page shows one row per bucket with its backend; callout 1 marks Buckets in the sidebar and callout 2 marks the downloads row.](/_/screenshots/route-bucket-buckets-row.webp)

3. Click **Advanced** inside the row.
4. In **Compression**, select **Off**.

   ![The downloads bucket is open with its Advanced settings; callout 3 marks Advanced and callout 4 marks the Compression list, set to Off.](/_/screenshots/bucket-compression-off.webp)

5. Click **Review & apply** in the bar at the bottom of the page. Check the diff in the dialog, and then click **Apply and Persist**.

   ![The review dialog shows that compression of the downloads bucket changes to false; the arrow points at Apply and Persist.](/_/screenshots/bucket-compression-apply-dialog.webp)

The **Compression** list has three options. **Inherit — global default (on)** follows the global setting and writes nothing into the bucket policy. **Always on** writes `compression: true`, and **Off** writes `compression: false`. When compression is off, the row hides **Delta size cutoff**, because the cutoff has no effect then.

## 2. Make the delta cutoff stricter

The proxy keeps a delta only when `delta_size / original_size` is below the cutoff. Otherwise it stores the full file. A lower cutoff keeps only the deltas that save a lot of space. The global default is `advanced.max_delta_ratio`, which is 0.75. To keep only the deltas that save at least half of the space in `releases`:

1. On **Storage → Buckets**, click the `releases` row to open it. Its **Advanced** settings are already open, because the bucket has a quota. If they are closed, click **Advanced**.
2. Type `0.5` in **Delta size cutoff**.

   ![The releases bucket is open with its Advanced settings; callout 1 marks the releases row and callout 2 marks Delta size cutoff, set to 0.5.](/_/screenshots/bucket-delta-cutoff.webp)

3. Click **Review & apply**, and then click **Apply and Persist**, as in the previous section.

Compression is a policy of the whole bucket, not of a prefix. If you want some prefixes compressed and others not, use one of these options:

- Split the data into two buckets with different compression settings. Both buckets can sit on the same backend.
- Rely on the cutoff. When the delta of a file is not worth keeping, the proxy stores the file as-is by itself, so you do not need to do anything.

## 3. Know what deltas well

Delta compression pays off when most bytes repeat across the stored versions: zipped releases, JARs and APKs, database dumps, tar archives, variants of an AI model, and game builds. On workloads with high similarity, these formats save 60 to 95% in practice. Archives that compress the whole stream (`.tar.gz`, `.tar.xz`, `.tar.zst`, solid `.7z`) usually do not delta, because one small change shifts the bytes through the rest of the stream. Container formats that compress each member on its own (`.zip`, `.jar`, `.docx`) usually do. The reason is in [delta compression](../explanation/delta-compression.md).

If you are not sure about your workload, test it with two real consecutive versions:

```bash
xdelta3 -D -e -s old-artifact new-artifact delta.vcdiff
# compare: stat -c%s delta.vcdiff  vs  stat -c%s new-artifact
```

Read the ratio against this rule of thumb:

| `delta / original` | Meaning |
|---:|---|
| `<= 0.20` | Excellent |
| `0.20–0.50` | Good |
| `0.50–0.80` | Marginal |
| `> 0.80` | Usually stored as-is |

## 4. Set a soft quota

In the admin UI:

1. On **Storage → Buckets**, click the `db-archive` row to open it.
2. Click **Advanced** inside the row.
3. Type `500` in **Quota**. The field counts in GiB (1 GiB is 1024³ bytes), and an empty field means no quota.

   ![The db-archive bucket is open with its Advanced settings; callout 1 marks the db-archive row, callout 2 marks Advanced, and callout 3 marks Quota, set to 500 GiB.](/_/screenshots/bucket-quota.webp)

4. Click **Review & apply**, and then click **Apply and Persist**.

The quota counts the bytes that the bucket really occupies on its storage backend. This total includes the shared delta baselines (`reference.bin`), not only the small deltas of each object, so a bucket that holds one build per folder is measured at its true size.

When a PUT request would push the bucket past its quota, the proxy rejects it with `403 AccessDenied` and a message that starts with `Bucket quota exceeded`. The proxy reads the running usage counter of the bucket, which it updates on every write and delete, so the check is almost immediate. The quota is still **soft**: requests that run at the same moment each check the counter before any of them stores its bytes, so a burst of concurrent writes can go slightly over the limit. If you need a strict hard cap, enforce it at the reverse proxy or at the storage provider.

The same page has one more limit, in the card **Object size limit**. **Maximum object size (MiB)** is the largest object that any bucket accepts (`advanced.max_object_size`, which the YAML file holds in bytes). The card has its own **Review & apply** bar. When you sign in to the admin UI, the upload page reads the quota of the bucket, its usage counter and the object size limit. It refuses a file that is bigger than the size limit, or that does not fit in the space that the quota leaves, before any byte goes out, and it says why. A files-only session cannot read these limits, so for such a session the check of the proxy is the only one.

## 5. Freeze a bucket

To make a bucket read-only, for example during a manual migration, set its quota to `0`. The proxy then rejects every upload to the bucket with `403 AccessDenied` and the message `Bucket is frozen (quota = 0)`. Reads and lists keep working. In the admin UI, type `0` in **Quota**. An empty **Quota** field removes the quota, so it does not freeze the bucket.

## The same change in YAML

The steps above write this configuration into the `storage` section of `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)):

```yaml
# validate
storage:
  buckets:
    downloads:
      compression: false              # every object stored as-is
    releases:
      max_delta_ratio: 0.5            # keep only deltas that save 50% or more
    db-archive:
      quota_bytes: 536870912000       # 500 GiB; 0 freezes the bucket
```

`quota_bytes` is in bytes, so 500 GiB is 500 × 1024³ = 536870912000. A bucket without `compression` and `max_delta_ratio` follows the global `advanced.max_delta_ratio`.

After you edit the file, restart the proxy, or apply the file to the running proxy with `POST /_/api/admin/config/apply` or with `deltaglider_proxy config apply deltaglider_proxy.yaml --server https://s3.acme.example`.

## Verify

1. Check that the proxy applied the policy:

   ```bash
   curl -b cookies https://s3.acme.example/_/api/admin/config/section/storage?format=yaml
   ```

2. Check that compression behaves as configured. Upload two versions of a file and check the `x-amz-storage-type` header of a HEAD response: `delta` means compressed, and `passthrough` means stored as-is. The proxy sends this header only when it runs with `DGP_DEBUG_HEADERS=true`. Without it, the `x-amz-meta-dg-note` header of the HEAD response carries the same value.

3. Check that the quota works. On a frozen bucket, a PUT request fails with `403`:

   ```bash
   aws --endpoint-url https://s3.acme.example s3 cp probe.txt s3://db-archive/probe.txt
   ```

4. Check the savings for each bucket. They show up on the dashboard at **Observability → Dashboard** (`/_/admin/dashboard`) and in the usage counter of each bucket. To read the counter, sign in with `POST /_/api/admin/login` to get an admin session cookie, and then run `curl -b /tmp/dgp.cookies 'https://s3.acme.example/_/stats?bucket=db-archive'`, or send `GET /_/api/admin/usage/bucket/db-archive` for the full counter row. The proxy updates the counter on every write. If the counter drifts, reconcile it with `POST /_/api/admin/usage/refresh?bucket=db-archive`.

## Related

- [Delta compression](../explanation/delta-compression.md): how routing, references and ratios work.
- [Your first delta savings](../tutorials/first-delta-savings.md): watch the ratio on a real upload.
- [Configuration reference](../reference/configuration.md): every `storage.buckets` field, including `public_prefixes` and `alias`.
- [How to expire and archive objects](expire-and-archive-objects.md): control the size by age instead of by a cap.
- [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md): how the admin UI and the YAML file relate.
