# DeltaGlider compression vs. S3 Object Versioning

*Two different things that both involve the word "version" — and why DeltaGlider deliberately does only one of them.*

DeltaGlider's whole pitch is "store a hundred versions, pay for one." S3 has a native feature literally called **Object Versioning**. They sound like the same thing. They are not, and conflating them will bite you in production — so this page draws the line plainly.

**Short answer up front:** DeltaGlider does **not** implement native S3 Object Versioning. It does not keep multiple historical states of a single key, it does not honour `?versionId=`, and it reports versioning as *not enabled* on every bucket. What it does instead is delta-*compress* the distinct objects you already store. If you rely on S3 versioning today — for rollback or ransomware protection — read the whole page, because the answer changes how you'd deploy.

## The two meanings of "version"

**S3 Object Versioning** keeps multiple immutable states *of the same key*. You `PUT s3://bucket/app.tar` ten times; with versioning enabled the bucket holds ten distinct versions of `app.tar`, each addressable by a `versionId`, and a `DELETE` adds a delete-marker rather than destroying data. It's a data-protection feature: undo, audit, and recovery from accidental or malicious overwrites.

**DeltaGlider compression** works on *distinct keys*. Your firmware pipeline writes `fw-1.4.0.tar`, `fw-1.4.1.tar`, `fw-1.4.2.tar` — three different keys that happen to be 99% identical. DeltaGlider stores the first as a baseline and the rest as tiny xdelta3 deltas against it. The "hundred versions" in the tagline are a hundred differently-named objects, not a hundred states of one name. See [how delta compression works](delta-compression.md) for the mechanics.

So the marketing "versions" means *release editions you name yourself*. The S3 API "versions" means *overwrite history of a single name*. DeltaGlider does the first and not the second.

## What DeltaGlider actually does with the versioning API

This is the precise behaviour, straight from the implementation — not aspiration:

- **`GetBucketVersioning`** returns an empty configuration for every bucket, which an S3 client reads as *versioning not enabled*. There is no per-bucket toggle to turn it on.
- **`PutBucketVersioning`** is not implemented. A client that tries to enable versioning gets a not-implemented error, not a silent success.
- **`ListObjectVersions`** is not implemented. There is no version history to enumerate.
- **`GET`/`HEAD` with `?versionId=`** — there are no stored versions, so there is nothing for a version id to address. A second `PUT` to the same key overwrites the object in the backend.
- **`CopyObject` with a `versionId` on the source** is explicitly rejected (`copy source versionId is not supported`).

In other words, the proxy presents the surface of a **non-versioned bucket**, consistently, on purpose. Every key holds exactly one current object.

## Why it's built this way

Delta compression and object versioning solve different problems, and stacking them naively would fight.

DeltaGlider's value comes from treating each key as a single object it can encode against a shared baseline. Native versioning would mean every key fans out into an unbounded chain of historical states, each of which would also want a delta relationship — a combinatorial mess that undermines the "one reference per deltaspace" model the savings depend on. Keeping the object model flat (one key, one current object) is what makes the storage math clean and the GET path byte-for-byte verifiable.

It's also honest about layering. If you want version history, the right place to keep it is usually the **backend**, not the proxy — and DeltaGlider is a control plane *over* your backend, not a replacement for it (see [multi-backend routing](multi-backend-architecture.md)).

## If you rely on S3 versioning for ransomware protection

This is the case that matters most, so it gets its own answer.

If your recovery story is "an attacker overwrites or deletes our objects, and we roll back to a prior version," **DeltaGlider does not provide that** at the proxy layer. A `PUT` over an existing key replaces it; a `DELETE` removes it. There is no proxy-held version history to roll back to.

What to do instead, in rough order of strength:

1. **Enable versioning and Object Lock on the upstream S3 backend directly.** Acme routes `releases` to `hetzner-fsn1`; if that provider supports bucket versioning / object lock, turn it on *there*. DeltaGlider stores each object as a normal backend object (a baseline or a `.delta`), so the backend's own versioning protects those stored bytes. Caveat: the backend versions the *encoded* form (the delta files), not your logical artifacts — recovery means restoring the backend objects, after which DeltaGlider reconstructs the logical objects from them.
2. **Replicate to an isolated DR backend.** Point a [replication rule](../reference/replication.md) at `aws-dr` (a separate account/provider) so a compromise of the primary doesn't reach the copy. Combine with credentials that can write-but-not-delete on the DR side.
3. **Back up the config and IAM state** with the [backup/restore](../how-to/back-up-and-restore.md) flow so the control plane itself is recoverable, independent of the data plane.

The honest summary: DeltaGlider is a storage-efficiency and control-plane layer, not a data-immutability layer. Put immutability where the bytes live.

## Quick reference

| Question | Answer |
|---|---|
| Does DeltaGlider support native S3 Object Versioning? | No. |
| Can I enable it per bucket? | No — `PutBucketVersioning` is not implemented. |
| Does `GetBucketVersioning` report enabled? | No — it reports not-enabled on every bucket. |
| Are `?versionId=` reads honoured? | No — there are no stored versions to address. |
| Does a second `PUT` to a key keep the old object? | No — it overwrites. |
| What are the "hundred versions" in the tagline, then? | A hundred *distinct keys* (release editions you name), delta-compressed against a baseline. |
| Where should version history / ransomware rollback live? | On the upstream backend (versioning + Object Lock) and/or an isolated DR replica. |

## Related

- [How delta compression works](delta-compression.md) — what the "versions" in the tagline actually are.
- [Multi-backend routing](multi-backend-architecture.md) — why the proxy is a control plane over your storage, not another store.
- [How migration works](how-migration-works.md) — moving an existing (possibly versioned) bucket onto the proxy.
- [Replication reference](../reference/replication.md) — isolating a DR copy.
