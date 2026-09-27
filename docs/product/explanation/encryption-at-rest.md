# About encryption at rest

Encryption at rest in DeltaGlider Proxy is a per-backend decision with four modes: `none`, `aes256-gcm-proxy`, `sse-kms`, and `sse-s3`. The configuration is small, but the reasoning behind the choice of mode, and behind the shape of the limits, is not. This page explains that reasoning.

## The threat model: what this defends, and what it cannot

At-rest encryption answers one question: if someone obtains the stored bytes without going through the proxy, can they read them? Examples are disk theft, a breached storage provider, a decommissioned drive that skipped the shredder, and a leaked S3 bucket. In all of these cases, the attacker has ciphertext and no key, and cannot recover the data. That is the whole promise of at-rest encryption.

At-rest encryption does *not* defend against the following:

- **A compromised proxy host.** In `aes256-gcm-proxy` mode the key lives in the proxy's runtime. An attacker who can read proxy memory or its configuration has the key. The native SSE modes change this, because the key lives in AWS. But then a different principal becomes the weak point (see below).
- **Compromised credentials at the right layer.** AWS transparently decrypts SSE-S3 objects for *any* IAM caller with `s3:GetObject`, so stolen AWS credentials read plaintext. SSE-KMS raises the bar (the caller also needs `kms:Decrypt`), but a compromised KMS principal can read everything. Proxy-AES is the inverse: AWS credentials are useless without the proxy's key, but the proxy's key is useless protection against the proxy itself.
- **The wire.** None of these modes encrypt transport. TLS is a separate concern with its own setup.
- **Metadata.** Object names, sizes, and user metadata are plaintext under every mode. A section below explains why.

There is no mode that defends against everything. The question is which party you trust least, and your choice of mode records that answer.

## The modes, and the real difference between them

Without the configuration details, the four modes differ on exactly one axis: who holds the key, and therefore who can read plaintext.

With `aes256-gcm-proxy`, the proxy encrypts object bodies with AES-256-GCM *before* the backend ever sees the bytes. The storage provider stores opaque ciphertext and has no path to plaintext. This is the mode for storage you don't fully trust. Acme's `db-archive` bucket is the typical example. It holds nightly database dumps that `backup-bot` writes, and it is routed to the `hetzner-fsn1` backend. That backend is budget S3-compatible storage that Acme chose for price, not for a compliance record. With proxy-AES on that backend, the provider (and anyone who breaches it, and any subpoena served on it) holds ciphertext. Someone reading the raw bucket sees a `DGE1` magic header, an IV, and length-prefixed encrypted chunks. The dumps are readable only through Acme's proxy. Delta compression runs *before* encryption, so the proxy keeps all of the storage savings on those highly-similar nightly dumps. The ciphertext of a delta is no bigger than the delta.

With `sse-kms`, the proxy delegates encryption to AWS and never touches key material; every write carries the SSE headers, and AWS does the rest. In exchange, you get the key management of AWS: per-key IAM, automated rotation, and a CloudTrail event for every decrypt. If an auditor asks "who read these objects in March," SSE-KMS can answer; proxy-AES cannot (the key never moves, so there is no per-decrypt event, and only the proxy's own access logs exist).

`sse-s3` is the budget version of the same idea: AWS-managed AES-256, no KMS cost, and no per-decrypt audit trail. AWS encrypts the objects on its disks and transparently decrypts them for any authorized IAM caller.

`none` is a legitimate choice, not a default that you forgot to change. Acme's `downloads` bucket serves public installers from its `public/` prefix. Encryption of world-readable artifacts only costs CPU and brings no threat-model benefit.

The decision comes down to two questions. First, is the backend storage untrusted (a third-party provider, a hostile jurisdiction, or a compliance rule that says "provider must not see plaintext")? Then use proxy-AES. It is also the only encrypting option for filesystem backends like `local-disk`, because the native modes work only on S3. Second, is the backend AWS, and is your compliance story AWS-native? Then use SSE-KMS if you need the audit trail and key lifecycle. Use SSE-S3 if the requirement is only "encrypted at rest: yes" on a checklist. Acme uses all three answers at once: proxy-AES on `hetzner-fsn1`, SSE-KMS on `aws-dr`, and no encryption on the public-CDN path. Encryption is per-backend so that one proxy can hold all of these postures at the same time.

## Why enabling isn't retroactive

Flipping a backend to `aes256-gcm-proxy` encrypts *new writes only*. Existing objects stay in their stored form. This surprises people, so this section explains why it is a design choice and not a gap.

Reads dispatch on a per-object metadata marker (`dg-encrypted`), not on the backend's current mode. The proxy serves an object without the marker as-is, and it decrypts an object with the marker. This makes it safe and instant to turn on encryption. Nothing breaks at flip time, you do not have to run a migration, and backends with mixed plaintext and ciphertext work. The alternative is to rewrite every existing object synchronously when you apply the config. That would turn a config change into an unbounded, failure-prone bulk operation hidden inside an apply button.

Instead, you bring history under the new mode with an explicit, visible operation: the **re-encrypt job**. It is a durable one-off job that rewrites every object whose markers do not match the configured mode of the backend. It resumes across restarts, and it gates writes to the bucket while it runs. The Backends page proposes one whenever you change encryption settings. See [how to encrypt data at rest](../how-to/encrypt-data-at-rest.md) for the procedure.

## Why in-place rotation is unsupported, and the shim that designs around it

A proxy-AES backend cannot simply swap its `key`. If the old key were gone, every object written under it would become unreadable. AES-GCM does not degrade gracefully: it authenticates or it fails. That is why a key change keeps the previous key in the legacy slot described below. Supporting transparent multi-key rotation would mean a key ring, per-object key resolution against an unbounded set, and a hidden pile of complexity in the hottest read path. The proxy does not accept that trade.

Instead, the proxy offers a deliberately minimal mechanism: a decrypt-only `legacy_key` shim that holds exactly one previous key generation. Every proxy-AES write stamps a `dg-encryption-key-id` on the object. Reads check the stamped id against the current key, then against the legacy slot. Writes never use the legacy key. So during a rotation, old objects stay readable while every new object lands under the new key, and the population converges in one direction only. Run the re-encrypt job to rewrite the remaining old objects, and then clear the shim. One legacy slot is a constraint, but it also forces each rotation to finish, so key generations do not pile up. The key-id mechanism also gives clear errors. A mismatch tells you *which* key an object wanted, instead of an opaque GCM authentication failure. The full procedure is in [how to rotate encryption keys](../how-to/rotate-encryption-keys.md).

**Key loss in proxy-AES mode is data loss.** The proxy does not escrow keys, and there is no recovery path. Back up the key off-box (in a secrets manager, a vault, or a sealed envelope) before the first encrypted write.

## Why metadata stays plaintext

Under every mode, including SSE-KMS, the backend stores object names, sizes, content-type, and `x-amz-meta-*` user metadata unencrypted. For the native modes, this is an AWS constraint: SSE encrypts only bodies. For proxy-AES the proxy mirrors that policy, partly for consistency, and partly because of a chicken-and-egg problem: the metadata *is* how the read path detects whether an object is encrypted at all. Encrypting the marker that says "this is encrypted" doesn't work.

As a consequence, an attacker with backend access learns names, approximate sizes (ciphertext length tracks plaintext length), and anything you put in metadata. If a value is secret, it belongs in the object body.

## The costs

Proxy-AES has concrete costs. **CPU and latency:** AES-256-GCM with hardware acceleration runs at roughly 1 to 3 GB/s per core, and a 100 MiB upload adds ~30 to 100 ms of proxy-side crypto. **Memory:** encrypted reads stream (≈130 KiB in flight regardless of object size), but an encrypted write of a body that the proxy holds in memory buffers the encrypted frames before handoff, so a 100 MiB `PutObject` can peak around 200 to 300 MiB RSS. Size your proxy accordingly, or pick a native mode, which moves all of this to AWS. **Disk:** a large upload or a multipart upload is stored from a file, and the proxy first encrypts that file into a temporary file in the spool directory (`DGP_SPOOL_DIR`). The temporary file counts against the spool budget (`DGP_SPOOL_MAX_BYTES`), in the same way as the files of the delta codec, so encrypted writes cannot fill the disk. When other requests use the whole budget, the proxy answers the upload with `503 SlowDown`, and S3 clients retry it. Range requests still work on encrypted objects, without the cost that you might expect. The chunked format locates the needed 64 KiB chunks in O(1), decrypts only those, and trims them. The overhead is at most one chunk of waste at each end.

## Who can read what, outside the proxy

In practice, the boundaries above mean that the proxy must be the write and read path for encrypted backends. The original Python DeltaGlider CLI speaks the same delta format but does **not** encrypt. If you point it at raw storage instead of the proxy, it writes plaintext into a bucket that you believed was encrypted. Conversely, raw `aws s3 cp` against an SSE-KMS bucket returns plaintext to any authorized caller, because that's what SSE-KMS *means*. Neither is a bug. Both show that the mode you picked defines exactly who can bypass the proxy and what they see. If the answer must be "nobody and nothing", use proxy-AES and point all clients at the proxy.

## Related

- How-to: [Encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- How-to: [Rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
- Reference: [Encryption](../reference/encryption.md)
