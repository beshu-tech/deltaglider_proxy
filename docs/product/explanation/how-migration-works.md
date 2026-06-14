# How migration works

*What actually happens when you move an existing bucket onto the proxy — and the questions you should ask before you do.*

Migrations are scary because the failure mode is "my data is now somewhere I don't understand." This page is the conceptual map so the [step-by-step guide](../how-to/migrate-existing-data-into-the-proxy.md) isn't a leap of faith. Three things up front, because they're the ones people worry about:

- **There is no lazy, magic, migrate-on-read step.** DeltaGlider never silently rewrites your existing objects in the background. Migration is an explicit action you run.
- **It does not require downtime** in the strict sense — both routes let the old bucket keep serving until you choose to cut over.
- **You pick the trade-off**: keep your existing layout untouched (no compression of the back-catalog), or do a one-time copy *through* the proxy that re-stores everything as deltas.

## The fork: in-place vs. copy-through

Every migration is one of two shapes. The guide calls them Route 1 and Route 2; here's what each one *is*.

### Route 1 — point at the bucket in place

You register your existing S3 bucket as a backend and route a proxy bucket to it. Nothing about the stored objects changes. The proxy reads and writes them as ordinary passthrough objects; your historical data stays exactly as it is on disk.

What this gives you: a zero-risk, zero-rewrite cutover. The proxy is now in the data path, so **new** uploads that land on a delta-eligible prefix start getting compressed. Your back-catalog does not shrink — it was written before the proxy existed and is left alone.

Use this when the existing data is fine as-is and you only care about compressing what comes next, or when you simply want the control plane (IAM, audit, replication) in front of storage you don't want to touch.

### Route 2 — copy through the proxy

You stand up a fresh proxy bucket (a new namespace) and do a one-time `sync` that *reads from the old location and writes through the proxy*. Because the proxy rebuilds each object on write, everything that lands this way is stored compressed — your version history itself becomes deltas.

What this gives you: the full storage savings on the existing catalog, not just future uploads. The cost is a one-time data movement (every object is read once and written once) and the disk/bandwidth to do it.

Use this when the back-catalog is the whole point — the firmware releases, the nightly dumps, the model checkpoints you're paying full price for today.

## What "rebuilds each object on write" means

This is the mechanic that makes Route 2 work, and it's worth understanding because it explains the one gotcha.

When an object is written through the proxy onto a delta-eligible prefix, the [PUT decision](delta-compression.md#the-put-decision) runs: the first object in a deltaspace becomes the **reference baseline**, and each later object is encoded as an xdelta3 delta against it. Whichever object arrives first is the baseline — so **upload order shapes your ratios**.

For versioned names this usually takes care of itself: `aws s3 sync` copies in key order, and `fw-2.3.0.tar`, `fw-2.4.0.tar`, `fw-2.5.0.tar` sort into version order anyway, so the oldest becomes the baseline and the newer ones delta cleanly against it. The case to watch is a prefix where the lexical order and the "most representative baseline" disagree; there, a poor baseline just means weaker ratios, never incorrect data — every reconstructed object is still SHA-256-verified byte-for-byte.

## Downtime, really?

Neither route forces a maintenance window:

- **Route 1**: the bucket you registered keeps serving throughout. You add the route, verify, then point clients at the proxy endpoint when you're ready. The switch is a client-side endpoint change, not a data operation.
- **Route 2**: the old bucket stays live and readable while the one-time sync runs into the *new* proxy namespace. You cut clients over only after the copy completes and you've verified it. If you need strict consistency for objects written *during* the sync, do a final catch-up sync after freezing writes — the usual dual-write / final-delta pattern, same as any bucket-to-bucket move.

What migration is *not*: it is not a trickle that rewrites objects as they're read, and it is not a background daemon quietly re-encoding your back-catalog. If the proxy didn't write an object, that object is unchanged.

## A related but different thing: backend-to-backend moves

Don't confuse *migrating onto the proxy* (the subject of this page) with *moving a bucket between backends once it's already on the proxy*. The latter is a first-class, resumable [migrate job](../reference/jobs.md) — stage, copy, verify, flip, cleanup — driven from the Jobs screen, with a write gate so the bucket stays consistent during the move. That's the [move-a-bucket guide](../how-to/move-a-bucket-between-backends.md). This page is about the one-time on-ramp from storage the proxy has never seen.

## Related

- [How to migrate an existing S3 bucket into the proxy](../how-to/migrate-existing-data-into-the-proxy.md) — the step-by-step for both routes.
- [How delta compression works](delta-compression.md) — the PUT decision and baseline mechanics referenced above.
- [How to move a bucket between backends](../how-to/move-a-bucket-between-backends.md) — the resumable job for relocating an already-onboarded bucket.
- [DeltaGlider compression vs. S3 Object Versioning](versioning-vs-s3-versioning.md) — what happens to a *versioned* source bucket.
