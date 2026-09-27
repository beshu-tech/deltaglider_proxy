# About jobs, write gates, and durability

DeltaGlider Proxy runs five kinds of background work (replication rules, lifecycle rules, bucket re-encryption, bucket migration, and metadata backfill) and presents all of them on one surface. This page explains why the proxy uses one surface for all five kinds, why some jobs block writes on purpose, and what "durable" means for a job.

## One surface for every kind

The operator's mental model should be: *anything that runs in the background is a job, and every job has runs, failures, and actions.* An operator asks the same questions about every job: is it running, when did it last run, what failed, and can I pause it? This is true whether the proxy is mirroring `releases` to `aws-dr`, expiring `db-archive` dumps after 90 days, or re-encrypting a bucket after a key change. Separate screens would mean several places to look during an incident, and slightly different words for "it's stuck." So the proxy has one jobs list and one runs/failures drawer. A per-kind capability matrix replaces one API per kind: you can pause a rule but not a migration, and you can cancel a migration but not a rule.

![The Jobs screen](/_/screenshots/jobs-screen.jpg)

## Rules vs one-offs

Two shapes exist because the work has two shapes. Rules (replication and lifecycle) are recurring policy. They belong in YAML, you review them in git, and they are identical across replicas. Their runtime state (cursors, pause flags, run history) lives separately in the config DB, so a config reload does not forget the work that is already done. One-offs (re-encrypt, migrate and metadata backfill) come from an operator action instead of a policy file. You create them through the API or the GUI, they live entirely in the DB, and each one *is* its own single run. A migration in YAML would mean that you commit a file to say "do this once, now". Replication in the DB would hide standing policy from code review. So each shape lives where its author works.

## Why maintenance jobs take a write gate

While a re-encrypt, migrate or metadata-backfill job works a bucket, S3 writes to that bucket get `503 SlowDown`. Reads pass untouched. The proxy chooses consistency over availability here on purpose.

The gate prevents a race. Suppose that a re-encrypt job sweeps `db-archive` after a key rotation while `backup-bot` PUTs tonight's dump. Without the gate, that PUT can land under the old configuration *after* the sweep passed its key. The job then finishes "successfully", and one object stays under a key that you are about to retire, with no error to tell you. A migration has the same race with a worse ending: a write to the old backend after the copy phase is lost when traffic flips.

The proxy uses `503 SlowDown` because it is a *protocol-native* refusal. AWS SDKs back off and retry automatically, so `backup-bot` waits instead of failing. The gate starts when the proxy creates the job, so there is no window between the decision and the claim. The proxy drains in-flight writes before it copies. The gate lifts at the moment a migration flips routing, before the optional source cleanup, so writes are unavailable only during the copy.

Soft quotas sit at the opposite end of the same trade. Bucket quotas read a running usage counter that the proxy updates after each write, so writes that run at the same moment each check the counter before the others have stored their bytes, and a burst of concurrent writes can overshoot the limit. Quotas are soft because a hard quota would put a synchronous full-bucket size check on every PUT. That check would slow down the hot path to enforce a budget number. Where the write gate buys strict consistency with temporary unavailability, soft quotas buy hot-path speed with approximate enforcement. If you need a hard cap, enforce it at the storage provider.

## What "durable" means here

Every job survives a proxy restart, because of the specific mechanisms below. Long-running work persists a continuation cursor, so a run that stops in the middle of a page resumes where it stopped instead of rescanning from the top. A one-shot guard restarts the run fresh exactly once if the stored cursor turns out to be poison. Leases stop two workers (for example, the scheduler and a run-now click) from running the same rule at the same time, and a lapsed lease never comes back to life. Where the lease lives decides how far this protection reaches. When a coordination bucket is configured, the replication lease is an S3 object that is written with conditional writes, so every instance sees it, and only one instance runs a rule. The lifecycle and maintenance leases live in the config DB of each instance, and the config sync does not carry them. On a multi-instance deployment, two instances can therefore run the same lifecycle rule at the same time. On boot, the proxy marks as failed every run that a dead process left in `running`, and it shows each one to the operator as a row. It also re-queues the pending maintenance jobs, so a restart in the middle of a migration resumes the migration and does not leave a half-moved bucket behind. The proxy treats a graceful stop as neither a success nor a failure. While the process shuts down, work that is still running can fail only because the runtime goes away, so the maintenance worker stops at an object boundary and hands the job back to the queue instead of settling it. A cancelled migration before the routing flip unwinds cleanly; the source is never deleted on a failed run.

## The event outbox: the durable journal underneath

Two features share one foundation: webhook and Slack notifications, and event-driven replication. Every successful object mutation appends a fact to a durable `event_outbox` table, and each consumer drains the table with its own cursor. The append is the only thing that the S3 write path does. Delivery never blocks a PUT, and a dead webhook endpoint cannot slow uploads. A missed event survives a crash, because it is a row in a table and not a message in memory. So replication can be "near-real-time" without being fragile: the outbox is the primary trigger, and a slow periodic reconcile sweeps up anything that a consumer missed.

## Lifecycle vs replication

Lifecycle and replication share the transfer machinery, but they make different promises. Replication continuously mirrors a live source. It is event-driven, with conflict policies and optional delete propagation, and it promises that the destination converges on the source. Lifecycle is age-driven housekeeping. It acts only on expired candidates: it deletes them or transitions them, and it can remove the source after a verified copy. It promises that old data goes away, or goes somewhere colder. Both copy through the engine, so multipart ETags, encryption routing, and compression behave the same way in both. They differ only in *when* they act and *what they promise about the destination*.

## Related

- How-to: [Move a bucket between backends](../how-to/move-a-bucket-between-backends.md)
- How-to: [Replicate a bucket](../how-to/replicate-a-bucket.md)
- Reference: [Jobs](../reference/jobs.md)
- Reference: [Event log](../reference/event-outbox.md)
