# About encryption at rest

Encryption at rest in DeltaGlider Proxy is a per-backend decision with four modes: `none`, `aes256-gcm-proxy`, `sse-kms`, and `sse-s3`. A bucket can replace the decision of its backend with its own proxy-AES key, or with no encryption. The configuration is small, but the reasoning behind the choice of mode, and behind the shape of the limits, is not. This page explains that reasoning.

## The threat model: what this defends, and what it cannot

At-rest encryption answers one question: if someone obtains the stored bytes without going through the proxy, can they read them? Examples are disk theft, a breached storage provider, a decommissioned drive that skipped the shredder, and a leaked S3 bucket. In all of these cases, the attacker has ciphertext and no key, and cannot recover the data. That is the whole promise of at-rest encryption.

At-rest encryption does *not* defend against the following:

- **A compromised proxy host.** In `aes256-gcm-proxy` mode the key lives in the proxy's runtime. An attacker who can read proxy memory or its configuration has the key. The native SSE modes change this, because the key lives in AWS. But then a different principal becomes the weak point (see below).
- **Compromised credentials at the right layer.** AWS transparently decrypts SSE-S3 objects for *any* IAM caller with `s3:GetObject`, so stolen AWS credentials read plaintext. SSE-KMS raises the bar (the caller also needs `kms:Decrypt`), but a compromised KMS principal can read everything. Proxy-AES is the inverse: AWS credentials are useless without the proxy's key, but the proxy's key is useless protection against the proxy itself.
- **The wire.** None of these modes encrypt transport. TLS is a separate concern with its own setup.
- **Names and sizes.** Object names, the content type and user metadata stay readable on the backend under every mode, and so does the size of most objects. A section below explains why. [About the encryption threat model](encryption-threat-model.md) lists, attacker by attacker, what proxy-AES protects and what it does not.

There is no mode that defends against everything. The question is which party you trust least, and your choice of mode records that answer.

## The modes, and the real difference between them

Without the configuration details, the four modes differ on exactly one axis: who holds the key, and therefore who can read plaintext.

With `aes256-gcm-proxy`, the proxy encrypts object bodies with AES-256-GCM *before* the backend ever sees the bytes. The storage provider stores opaque ciphertext and has no path to plaintext. This is the mode for storage you don't fully trust. Acme's `db-archive` bucket is the typical example. It holds nightly database dumps that `backup-bot` writes, and it is routed to the `hetzner-fsn1` backend. That backend is budget S3-compatible storage that Acme chose for price, not for a compliance record. With proxy-AES on that backend, the provider (and anyone who breaches it, and any subpoena served on it) holds ciphertext. Someone who reads the raw bucket sees a `DGE2` magic header, a key fingerprint, a random salt, and encrypted frames of 64 KiB. The dumps are readable only through Acme's proxy. Delta compression runs *before* encryption, so the proxy keeps all of the storage savings on those highly-similar nightly dumps. The ciphertext of a delta is no bigger than the delta.

With `sse-kms`, the proxy delegates encryption to AWS and never touches key material; every write carries the SSE headers, and AWS does the rest. In exchange, you get the key management of AWS: per-key IAM, automated rotation, and a CloudTrail event for every decrypt. If an auditor asks "who read these objects in March," SSE-KMS can answer; proxy-AES cannot (the key never moves, so there is no per-decrypt event, and only the proxy's own access logs exist).

`sse-s3` is the budget version of the same idea: AWS-managed AES-256, no KMS cost, and no per-decrypt audit trail. AWS encrypts the objects on its disks and transparently decrypts them for any authorized IAM caller.

`none` is a legitimate choice, not a default that you forgot to change. Acme's `downloads` bucket serves public installers from its `public/` prefix. Encryption of world-readable artifacts only costs CPU and brings no threat-model benefit.

The decision comes down to two questions. First, is the backend storage untrusted (a third-party provider, a hostile jurisdiction, or a compliance rule that says "provider must not see plaintext")? Then use proxy-AES. It is also the only encrypting option for filesystem backends like `local-disk`, because the native modes work only on S3. Second, is the backend AWS, and is your compliance story AWS-native? Then use SSE-KMS if you need the audit trail and key lifecycle. Use SSE-S3 if the requirement is only "encrypted at rest: yes" on a checklist. Acme uses all three answers at once: proxy-AES on `hetzner-fsn1`, SSE-KMS on `aws-dr`, and no encryption on the public-CDN path. Encryption is per-backend so that one proxy can hold all of these postures at the same time.

## Why a bucket can have its own key

A backend is a place to store bytes, and a bucket is a data set. Two data sets on one backend can need different treatment. Acme keeps `releases` and `db-archive` on the same `local-disk` backend. The dumps in `db-archive` hold customer data, and Acme wants them under a key that only the database team rotates. The release builds in `releases` need no such key. Before per-bucket encryption, Acme had to define a second backend on the same disk only to give `db-archive` another key.

A bucket `encryption` block gives the bucket its own proxy-AES key, or no encryption, on a shared backend. The block belongs to the stored bucket, which is the backend and the real bucket name. The proxy decides the key by the stored bucket, not by the name in the request, because two names can reach one stored bucket through an alias. If the two names had different keys, one deltaspace would hold deltas and references under two keys, and nobody could tell which name wrote which object. A bucket key id is always explicit, so that the admin UI and the audit log show each bucket key under a name that someone chose.

## The keyring: how a bucket reads what it wrote before

Each backend has one encrypting layer. That layer holds a keyring: every key that the config gives to the backend, which is the key and legacy key of the backend and the key and legacy key of each bucket on it. Each encrypted object carries the fingerprint of the key that wrote it: 8 bytes that the proxy derives from the key, and that reveal nothing practical about it. When the proxy reads the object, it looks up that fingerprint in the keyring. The key id in the configuration is only the name under which the proxy shows the key, so one key has one id on a backend.

This is what makes a change of key safe. When `db-archive` gets its own key, its existing objects and its delta references are still under the backend key. Their key is in the keyring, so they stay readable. A new delta in an existing deltaspace is computed against the old reference (read with the backend key) and stored under the bucket key. The re-encrypt job later rewrites the old objects and references under the bucket key.

The keyring is bounded by the config: it never holds a key that the YAML does not name. When an object names a fingerprint that the keyring does not have, the read fails with an error that names the fingerprint, and the proxy never tries keys at random.

## What is encrypted, per bucket

With a bucket key, the bodies of the bucket's objects, of their deltas and of the bucket's delta references are encrypted under that key, and so are the facts of their sealed metadata (see below). The other buckets on the backend are not affected: they keep the encryption of the backend, or their own block.

A bucket block can also use native SSE (`sse-kms`, `sse-s3`) when the bucket is on an S3 backend; a filesystem backend would store the bytes as they are, so it refuses them. The YAML config is per node, so with several instances two nodes with different blocks could write one bucket under two keys. The encryption witness in the sync bucket detects that: an instance that disagrees about a bucket refuses writes to it.

## Why enabling isn't retroactive

Flipping a backend to `aes256-gcm-proxy` encrypts *new writes only*. Existing objects stay in their stored form. This surprises people, so this section explains why it is a design choice and not a gap.

Reads dispatch on the object, not on the backend's current mode: the proxy decrypts an object that carries sealed metadata, whatever the bucket does today. This lets a bucket hold objects under an old key and a new key at the same time, so a key change never needs a migration at flip time. The alternative is to rewrite every existing object synchronously when you apply the config. That would turn a config change into an unbounded, failure-prone bulk operation hidden inside an apply button.

Plaintext history is the one case that needs a decision. In a bucket that encrypts, an object without encryption can be the history of the bucket, or it can be an object that someone with write access to the backend planted. Its bytes look the same in both cases, and nothing on the object can prove which case it is. The proxy therefore refuses such an object by default (`plaintext_reads: refuse`), because an encrypting bucket must not serve data that the proxy did not write. When you start to encrypt a bucket that already holds objects, you set `plaintext_reads: serve` for the time of the conversion. With `serve`, the proxy accepts the plaintext objects as they are, so you decide for how long that trust lasts.

You bring history under the new mode with an explicit, visible operation: the **re-encrypt job**. It is a durable one-off job that rewrites every object whose encryption does not match the configuration of its bucket. It resumes across restarts, and it gates writes to the bucket while it runs. The job reads through the same rule as every other read, so it converts plaintext objects only under `serve`. When the job succeeded, you remove `plaintext_reads: serve`, and from then on every object in the bucket must carry a valid seal. After an encryption change, the admin UI proposes a job for each bucket whose effective encryption changed: on the Backends page for a backend change, and in the bucket row for a bucket change. See [how to encrypt data at rest](../how-to/encrypt-data-at-rest.md) for the procedure.

## Why in-place rotation is unsupported, and the shim that designs around it

A proxy-AES backend cannot simply swap its `key`. If the old key were gone, every object written under it would become unreadable. AES-GCM does not degrade gracefully: it authenticates or it fails. That is why a key change keeps the previous key in the legacy slot described below. Transparent rotation through an unbounded set of old keys would put a growing pile of key generations in the hottest read path. The proxy does not accept that trade: the keyring holds only the keys that the config names.

Instead, the proxy offers a deliberately minimal mechanism: a decrypt-only `legacy_key` shim that holds exactly one previous key generation per backend, and one per bucket that has its own key. Every proxy-AES object carries the fingerprint of its key. A read finds the current key or the legacy key by that fingerprint. Writes never use the legacy key. So during a rotation, old objects stay readable while every new object lands under the new key, and the population converges in one direction only. Run the re-encrypt job to rewrite the remaining old objects, and then clear the shim. One legacy slot is a constraint, but it also forces each rotation to finish, so key generations do not pile up. The fingerprint also gives clear errors. A missing key is reported with the fingerprint that the object wanted, instead of an opaque GCM authentication failure. Each key also has a key commitment in every object, so a wrong key can never decrypt an object into garbage. The full procedure is in [how to rotate encryption keys](../how-to/rotate-encryption-keys.md).

**Key loss in proxy-AES mode is data loss.** There is no recovery path without the key. A full backup carries a key that is in the config file, but not a key that an environment variable supplies, and a backup on the same host is lost with the host. Back up the key off-box (in a secrets manager, a vault, or a sealed envelope) before the first encrypted write.

## What the sealed metadata protects, and what stays readable

An encrypted object needs metadata that the backend can read. S3 returns the content type and the user metadata in its own headers, a LIST shows names and stored sizes, and the read path must find the key before it can decrypt anything. The native SSE modes encrypt only bodies, so under them the backend stores every fact of an object in clear.

Proxy-AES goes further, with *sealed metadata*. The facts that tell something about the content are encrypted under a key of the object: the size of a delta's object, the MD5 and the SHA-256 of every object, and the SHA-256 of a delta's reference. The seal also holds the size of every object and its creation time, so a writer on the backend cannot change them, although the clear copy of the creation time and the stored size stay readable. The seal is also authenticated together with the location of the object, which is the real bucket name, the folder and the name. A writer on the backend therefore cannot change a fact, move an object to another name, or swap two objects without the proxy noticing it. The seal also holds a digest of the content type and of the user metadata, so an edit of these clear fields fails the read too. That stops, for example, a changed `Content-Type` that would make a browser run a stored file as HTML.

Some facts stay readable, because the format cannot hide them cheaply. The names of objects and folders stay in clear, because S3 lists and sorts by name. The stored size of an object stored as it is gives its exact plaintext size, because every object uses the same framing. The size of a delta stays readable, and it tells how similar a new object is to the reference of its folder. The fingerprint of the key tells which objects share a key. [About the encryption threat model](encryption-threat-model.md) goes through these limits for each kind of attacker. If a value is secret, keep it in the object body, not in its name or in its user metadata.

## The costs

Proxy-AES has concrete costs. **CPU and latency:** AES-256-GCM with hardware acceleration runs at roughly 1 to 3 GB/s per core, and a 100 MiB upload adds ~30 to 100 ms of proxy-side crypto. **Memory:** encrypted reads stream (≈130 KiB in flight regardless of object size), but an encrypted write of a body that the proxy holds in memory buffers the encrypted frames before handoff, so a 100 MiB `PutObject` can peak around 200 to 300 MiB RSS. Size your proxy accordingly, or pick a native mode, which moves all of this to AWS. **Disk:** a large upload or a multipart upload is stored from a file, and the proxy first encrypts that file into a temporary file in the spool directory (`DGP_SPOOL_DIR`). The temporary file counts against the spool budget (`DGP_SPOOL_MAX_BYTES`), in the same way as the files of the delta codec, so encrypted writes cannot fill the disk. When other requests use the whole budget, the proxy answers the upload with `503 SlowDown`, and S3 clients retry it. Range requests still work on encrypted objects, without the cost that you might expect. The format locates the needed 64 KiB frames in O(1), reads only those frames in one request, decrypts them, and trims them. The overhead is at most one frame of waste at each end.

## Who can read what, outside the proxy

In practice, the boundaries above mean that the proxy must be the write and read path for encrypted backends. The original Python DeltaGlider CLI speaks the same delta format but does **not** encrypt. If you point it at raw storage instead of the proxy, it writes plaintext into a bucket that you believed was encrypted. The proxy then refuses to serve those objects, unless the bucket has `plaintext_reads: serve`, because it cannot tell them from objects that an attacker planted. Conversely, raw `aws s3 cp` against an SSE-KMS bucket returns plaintext to any authorized caller, because that's what SSE-KMS *means*. Neither is a bug. Both show that the mode you picked defines exactly who can bypass the proxy and what they see. If the answer must be "nobody and nothing", use proxy-AES and point all clients at the proxy.

## Related

- Explanation: [About the encryption threat model](encryption-threat-model.md)
- How-to: [Encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- How-to: [Rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
- Reference: [Encryption](../reference/encryption.md)
