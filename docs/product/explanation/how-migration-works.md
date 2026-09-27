# How migration works

*What happens when you move an existing bucket onto the proxy, and the questions to ask before you do it.*

A migration feels risky when you cannot tell where your data ends up. This page explains the concepts behind the [step-by-step guide](../how-to/migrate-existing-data-into-the-proxy.md), so that you know what each step does. Three facts come first, because people worry about them most:

- **There is no lazy migrate-on-read step.** DeltaGlider never rewrites your existing objects in the background. Migration is an explicit action that you run.
- **It does not require downtime** in the strict sense. With both routes, the old bucket keeps serving until you choose to cut over.
- **You pick the trade-off.** You can keep your existing layout untouched, and the back-catalog then gets no compression. Or you can do a one-time copy *through* the proxy, which stores everything again as deltas.

## The fork: in-place vs. copy-through

Every migration has one of two shapes. The guide calls them Route 1 and Route 2. This section explains what each one *is*.

### Route 1: point at the bucket in place

You register your existing S3 bucket as a backend and route a proxy bucket to it. The stored objects do not change. The proxy reads and writes them as ordinary passthrough objects, and your historical data stays exactly as it is on disk.

This route gives you a cutover with no risk and no rewrite. The proxy is now in the data path, so the proxy compresses **new** uploads that land on a delta-eligible prefix. Your back-catalog does not shrink, because it was written before the proxy existed and the proxy leaves it alone.

Use this route when the existing data is fine as it is and you want to compress only the uploads that come next. Also use it when you want the control plane (IAM, audit, replication) in front of storage that you do not want to touch.

### Route 2: copy through the proxy

You create a fresh proxy bucket (a new namespace) and run a one-time `sync` that *reads from the old location and writes through the proxy*. The proxy rebuilds each object on write, so it stores everything that arrives this way in compressed form. Your version history itself becomes deltas.

This route gives you the full storage savings on the existing catalog as well as on future uploads. The cost is a one-time data movement (the sync reads every object once and writes it once), plus the disk and bandwidth for it.

Use this route when the back-catalog is the reason for the migration: for example the firmware releases, the nightly dumps, or the model checkpoints that you pay full price for today.

## What "rebuilds each object on write" means

This mechanism makes Route 2 work, and it also explains the one surprise of Route 2.

When a client writes an object through the proxy onto a delta-eligible prefix, the [PUT decision](delta-compression.md#the-put-decision) runs. The first object in a deltaspace becomes the **reference baseline**, and the proxy encodes each later object as an xdelta3 delta against it. The object that arrives first is the baseline, so **upload order shapes your ratios**.

For versioned names, this usually works out without effort. `aws s3 sync` copies in key order, and `fw-2.3.0.tar`, `fw-2.4.0.tar`, `fw-2.5.0.tar` sort into version order, so the oldest file becomes the baseline and the newer ones delta cleanly against it. Watch out for a prefix where the lexical order and the most representative baseline disagree. There, a poor baseline gives weaker ratios, but it never gives incorrect data, because the proxy verifies every reconstructed object byte for byte with SHA-256.

## Downtime

Neither route forces a maintenance window:

- **Route 1**: the bucket that you registered keeps serving the whole time. You add the route, verify it, and then point clients at the proxy endpoint when you are ready. The switch is an endpoint change on the client side, not a data operation.
- **Route 2**: the old bucket stays live and readable while the one-time sync runs into the *new* proxy namespace. You cut clients over only after the copy completes and you verify it. If you need strict consistency for objects that clients write *during* the sync, freeze writes and then run a final catch-up sync. This is the usual dual-write and final-delta pattern of any bucket-to-bucket move.

Migration does not rewrite objects as clients read them, and no background daemon re-encodes your back-catalog. If the proxy did not write an object, that object is unchanged.

## A related but different thing: backend-to-backend moves

*Migrating onto the proxy* (the subject of this page) is different from *moving a bucket between backends when the bucket is already on the proxy*. For the second case, the proxy has a resumable [migrate job](../reference/jobs.md) with the phases stage, copy, verify, flip and cleanup. You start it from the Jobs screen, and a write gate keeps the bucket consistent during the move. The [move-a-bucket guide](../how-to/move-a-bucket-between-backends.md) covers it. This page is about the one-time move from storage that the proxy has never seen.

## Related

- [How to migrate an existing S3 bucket into the proxy](../how-to/migrate-existing-data-into-the-proxy.md): the step-by-step for both routes.
- [How delta compression works](delta-compression.md): the PUT decision and the baseline mechanics that this page refers to.
- [How to move a bucket between backends](../how-to/move-a-bucket-between-backends.md): the resumable job that moves a bucket that is already on the proxy.
- [DeltaGlider compression vs. S3 Object Versioning](versioning-vs-s3-versioning.md): what happens to a *versioned* source bucket.
