# How to upgrade to 2.0

This guide takes a proxy that runs 1.19 to 2.0.0. Version 2.0.0 is a major release because some changes stop a setup that worked on 1.19. Do the steps in this order. The [general upgrade guide](upgrade.md) describes the backup, swap and verify routine that every upgrade uses, and the [changelog](../changelog.md) lists every change in the release.

## Before you start

Read the whole guide once. Most installs need only steps 1, 2 and 7. Steps 3 to 6 apply only when your setup uses the feature that the step names.

You cannot go back to 1.19 by starting the old binary on the new files. The first start of 2.0 moves the config database schema from version 24 to version 29, and a 1.19 binary refuses a database with a newer schema. To go back, you put back the copy that you make in step 1.

## 1. Back up the config database and its key

The config database `deltaglider_config.db` holds the IAM users, the groups, the OAuth providers and the mapping rules. It lives in the directory of the config file (the directory of `DGP_CONFIG`), or in the working directory of the proxy when `DGP_CONFIG` is not set.

1. Make a full backup from the admin UI, or with a request to `GET /_/api/admin/backup` (see [Back up and restore](back-up-and-restore.md)).
2. Stop the proxy, and copy `deltaglider_config.db`, the YAML config file and the `.deltaglider_bootstrap_hash` file (when it exists) to a safe place. This copy is the only way back to 1.19.

In 1.19, the bootstrap password hash was the encryption key of the config database. In 2.0, the database has its own key. The first start of 2.0 re-encrypts the database with `DGP_CONFIG_DB_KEY`. When that variable is not set, the proxy generates a random key, writes it to the file `deltaglider_config.db.key` next to the database (mode 0600), and uses that key. The re-encryption works on a copy, and the copy replaces the original only after it opens with the new key, so a failed start leaves the original unchanged. From now on, back up `deltaglider_config.db.key` together with the database, because the database cannot be read without its key.

## 2. Check that the spool directory is writable

In 1.19, `DGP_SPOOL_THRESHOLD_BYTES` defaulted to `max_object_size`, so the spool was never used under the default config. In 2.0, the default is 16 MiB, or `max_object_size` when that is smaller. A delta object that is larger than the threshold is reconstructed into a file in `DGP_SPOOL_DIR`, and a delta-eligible upload that is larger than the threshold is encoded from a file there.

Make sure that the process can write to `DGP_SPOOL_DIR`, and that the volume has room for `DGP_SPOOL_MAX_BYTES`. To keep the 1.19 behaviour, set `DGP_SPOOL_THRESHOLD_BYTES` to the value of `max_object_size`. The [configuration reference](../reference/configuration.md) lists the spool settings.

## 3. Set the trusted proxy networks (reverse proxies only)

This step applies when you set `DGP_TRUST_PROXY_HEADERS=true`.

In 1.19, the rate limiter and the IP binding of admin sessions used the first `X-Forwarded-For` address. The client writes that header, so any client could pick the address that the proxy saw. In 2.0, the proxy trusts the forwarded headers only from the networks in `DGP_TRUSTED_PROXY_CIDRS`. The proxy refuses to start when `DGP_TRUST_PROXY_HEADERS=true` is set and `DGP_TRUSTED_PROXY_CIDRS` holds no valid network.

Set `DGP_TRUSTED_PROXY_CIDRS` to the networks of your reverse proxies or load balancers, as a comma-separated list. For a reverse proxy on the same host, the value is `127.0.0.1/32`.

```bash
DGP_TRUST_PROXY_HEADERS=true
DGP_TRUSTED_PROXY_CIDRS=10.0.0.0/8
```

## 4. Prepare every instance (config sync only)

This step applies when `config_sync_bucket` (or `DGP_CONFIG_SYNC_BUCKET`) is set, which means that several instances share one IAM database through a bucket.

1. Generate one key with `openssl rand -hex 32`, and set it as `DGP_CONFIG_DB_KEY` on every instance. An instance that has a sync bucket and no `DGP_CONFIG_DB_KEY` refuses to start.
2. Set `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true` on every instance for the rollout. An old instance encrypts its upload with the bootstrap password hash, and without this variable a 2.0 instance refuses a synced copy that opens only with the hash.
3. Upgrade all instances together, and make no IAM changes during the rollout. An instance that still runs 1.19 cannot read an upload that a 2.0 instance encrypted with the new key.
4. Remove `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC` when every instance runs 2.0, and restart them. The hash is in configuration files and backups, so a proxy that accepts a hash-encrypted copy lets anyone who can write to the bucket and knows the hash plant an IAM database.

Two more changes apply to multi-instance deployments:

- Every S3 backend that holds a bucket that clients can write to must support conditional writes. At start, the proxy tests each such backend, and it exits when a backend definitely does not support them. Backblaze B2 is one such backend. A bucket that only replication writes to can stay on it when you mark it `replication_target_only`. See [Use non-CAS backends safely](backend-capability-validation.md).
- A bucket migrate answers `409 Conflict` while a sync bucket is set. The migrate changes the routing of one instance only, and config sync does not carry routing. Move a bucket on a single instance, as [Move a bucket between backends](move-a-bucket-between-backends.md) describes.

The first sync after the upgrade has no merge base. The merge therefore keeps every row of both sides, so a delete that was not synced before the upgrade comes back. Check the users and groups after the rollout. The page [Run multiple instances](run-multiple-instances.md) describes the sync.

## 5. Upgrade the Kubernetes operator (operator only)

Operator 0.3.0 adds `spec.router.trustedProxyCidrs` to the `DeltaGliderProxy` custom resource. Its default is every private network range. Apply the new custom resource definition before you upgrade the operator, because the old definition does not know the new field:

```bash
kubectl apply -f operator/deploy/crd.yaml
kubectl apply -f operator/deploy/operator.yaml
```

With `bootstrapPassword.autoGenerate`, the operator adds a `dbKey` to the `<name>-bootstrap` Secret and gives it to every pod as `DGP_CONFIG_DB_KEY`. Without `autoGenerate`, add `DGP_CONFIG_DB_KEY` to the env Secret yourself. The operator does not scale beyond one pod without it. With the Helm chart, set `auth.configDbKey` (or `DGP_CONFIG_DB_KEY` in `auth.existingSecret`) before you raise `replicaCount` with config sync.

## 6. Check your scripts and monitoring

Some answers changed. Search your scripts, alerts and clients for these cases:

| Area | 1.19 | 2.0 |
|---|---|---|
| Admin API with no config DB open | `404 config DB not available` | `503 config DB not available` |
| `POST /_/api/admin/config/validate` | Accepted some documents that the apply refuses | Refuses what `/config/apply` refuses, with the same warnings |
| `deltaglider_proxy config lint` | Accepted an empty file and an unknown key in a flat file | Exits with code 4 for both |
| Default log level | `deltaglider_proxy=debug,tower_http=debug` | `deltaglider_proxy=info,tower_http=info` |
| ListObjectsV2 `NextContinuationToken` | The raw key of the last entry | `dg1.` followed by the base64url form of the key |

A client that sends the continuation token back unchanged sees no difference. A client that reads the key out of the token must stop doing so. This release still accepts a token in the old form, and each such request adds one to the counter `deltaglider_list_legacy_continuation_tokens_total`. A later release refuses the old form, so watch that counter before you upgrade again.

To get the old log output, set `advanced.log_level` or `DGP_LOG_LEVEL` to `deltaglider_proxy=debug,tower_http=debug`.

## 7. Start 2.0 and verify

1. Start the new image or binary with the same data volume and the same environment, plus the variables from the steps above.
2. Read the start log. When the proxy refuses to start, the log names the reason, for example a missing `DGP_TRUSTED_PROXY_CIDRS` or a backend without conditional writes.
3. Send a request to `GET /_/ready`. It answers `200` when the backends and the config database answer, and `503` when they do not.
4. Log in to the admin UI, and check that your users and groups are there.
5. List a bucket with your usual client, for example `aws --endpoint-url https://s3.acme.example s3 ls s3://releases/`.
6. With config sync, send a request to `GET /_/api/admin/config/sync` on each instance. Check that `healthy` is `true`. When it is `false`, `pull_error` and `push_error` say why.
7. Make a new full backup. It is the first backup that holds the new key.

## Go back to 1.19

A downgrade to 1.19 is not supported. If 2.0 does not start and you cannot fix the cause, this procedure puts back the state from before the upgrade.

1. Stop the 2.0 proxy.
2. Put back the files that you copied in step 1: `deltaglider_config.db`, the YAML config file and `.deltaglider_bootstrap_hash`. Remove `deltaglider_config.db.key`.
3. Start the 1.19 image or binary.

IAM changes that you made on 2.0 are lost, because the old copy does not hold them.
