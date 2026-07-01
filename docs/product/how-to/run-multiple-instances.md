# How to run multiple instances (HA)

This guide shows you how to run more than one DeltaGlider Proxy instance against the same storage, with IAM state kept in sync via a shared S3 bucket.

The thing that needs coordinating is the encrypted config DB (`deltaglider_config.db`) — IAM users, groups, OAuth providers. Object data needs nothing: all instances route to the same backends. Why it's built this way: [Multi-backend architecture](../explanation/multi-backend-architecture.md).

## 1. Point every instance at a sync bucket

Set the same sync bucket on every instance, in YAML or env:

```yaml
advanced:
  config_sync_bucket: dgp-iam-sync
```

```bash
DGP_CONFIG_SYNC_BUCKET=dgp-iam-sync
```

After every IAM mutation, the mutating instance uploads the encrypted DB to the bucket. The other instances poll every 5 minutes and download when the ETag changes. All instances must share the same bootstrap password — it's the DB encryption key.

Update uploads use `If-Match` compare-and-swap protection by default. If your S3-compatible endpoint rejects conditional update PUTs and you have exactly one writer, you may set `advanced.config_sync_update_cas: false` or `DGP_CONFIG_SYNC_UPDATE_CAS=false`. With that opt-out, an existing remote DB that the local bootstrap key cannot adopt is also treated as the target for a single-writer replacement upload. Do not disable update CAS when more than one instance can mutate IAM.

## 2. Designate one writer

Sync is **not multi-master**. Run exactly one instance as the IAM administration surface (where operators use the admin GUI / admin API); treat the others as readers. If two instances both mutate, the "loudest" writer wins and the other's mutations are lost — you'll see continuous `[config-sync] ETag mismatch on DB download — retrying` in the logs. An occasional mismatch is normal (two events inside one 5-minute poll window resolve on the next cycle); a continuous one means you have two writers.

If you want no writer at all, switch to `iam_mode: declarative` and manage IAM via YAML + GitOps — see [How to manage IAM as code](manage-iam-as-code.md).

## 3. Force a sync when you can't wait

After a known-good mutation on the writer, make a reader pull immediately instead of waiting out the poll interval:

```bash
curl -b cookies -X POST https://dgp-reader-1:9000/_/api/admin/config/sync-now
```

Use this during rollouts and incident response — e.g. you just disabled a leaked key on the writer and want every reader to enforce it now.

## 4. If you scale with Helm

`replicaCount` defaults to `1` — do not raise it until the sync bucket is configured. Each pod with its own independent `/data/deltaglider_config.db` is an independent control plane: replication and lifecycle runs are guarded by per-rule database leases (`lease_ttl: "60s"`, `heartbeat_interval: "20s"`), and the lease guard only coordinates when every replica sees the same durable DB state. With sync configured, a dead runner's lease becomes stealable after roughly a minute.

See [How to deploy on Kubernetes with Helm](deploy-on-kubernetes.md) for the chart specifics.

## 5. Mind upgrades across the fleet

During a rolling upgrade, a newer binary may migrate the DB schema forward; older instances still running will download a DB they can't fully read. Upgrade all instances before making IAM mutations, or accept that mid-rollout mutations are lost on older readers. Details: [How to upgrade the proxy](upgrade.md).

## Verify

```bash
# 1. On the writer: create a throwaway user (admin GUI or API), then force a pull on a reader
curl -b cookies -X POST https://dgp-reader-1:9000/_/api/admin/config/sync-now

# 2. The reader sees the new user
curl -b cookies https://dgp-reader-1:9000/_/api/admin/users | jq '.[] | .name'

# 3. The new user's credentials work against the reader
aws s3 ls --endpoint-url https://dgp-reader-1:9000
```

Watch the reader's logs for `[config-sync]` lines — a download on ETag change is the success signal; continuous ETag-mismatch retries mean two writers.

## Related

- [How to back up and restore](back-up-and-restore.md) — sync replicates state; it does not protect it
- [How to manage IAM as code](manage-iam-as-code.md) — the GitOps alternative to a designated writer
- [How to monitor with Prometheus and Grafana](monitor-with-prometheus.md) — scraping multiple targets
- [Configuration reference](../reference/configuration.md) — config-sync fields
