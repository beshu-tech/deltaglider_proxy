# About encryption at rest

Encryption at rest in DeltaGlider Proxy is a per-backend decision with four modes: `none`, `aes256-gcm-proxy`, `sse-kms`, and `sse-s3`. The configuration is small; the reasoning behind which mode to pick, and why the limits are shaped the way they are, is not. This page is the reasoning.

## The threat model — what this defends, and pointedly what it can't

At-rest encryption answers one question: **if someone obtains the stored bytes without going through the proxy, can they read them?** Disk theft, a breached storage provider, a decommissioned drive that skipped the shredder, a leaked S3 bucket — in all of these the attacker has ciphertext and no key, and the data is unrecoverable. That's the whole promise, and it's a real one.

It is equally important to be clear about what at-rest encryption does *not* defend:

- **A compromised proxy host.** In `aes256-gcm-proxy` mode the key lives in the proxy's runtime. An attacker who can read proxy memory or its configuration has the key. The native SSE modes shift this — the key lives in AWS — but then a different principal becomes the weak point (see below).
- **Compromised credentials at the right layer.** SSE-S3 objects are transparently decrypted by AWS for *any* IAM caller with `s3:GetObject` — so stolen AWS credentials read plaintext. SSE-KMS raises the bar (the caller also needs `kms:Decrypt`), but a compromised KMS principal is game over. Proxy-AES is the inverse: AWS credentials are useless without the proxy's key, but the proxy's key is useless protection against the proxy itself.
- **The wire.** None of these modes encrypt transport. TLS is a separate concern with its own setup.
- **Metadata.** Object names, sizes, and user metadata are plaintext under every mode — more on why below.

There is no mode that defends against everything. The question is which party you trust least, and that's what mode selection actually encodes.

## The modes, and the real difference between them

Strip away the configuration details and the four modes differ on exactly one axis: **who holds the key, and therefore who can read plaintext.**

With `aes256-gcm-proxy`, the proxy encrypts object bodies with AES-256-GCM *before* the backend ever sees the bytes. The storage provider stores opaque ciphertext and has no path to plaintext, ever. This is the mode for storage you don't fully trust. Acme's `db-archive` bucket is the canonical story: nightly database dumps written by `backup-bot`, routed to the `hetzner-fsn1` backend — budget S3-compatible storage chosen for price, not for a compliance pedigree. With proxy-AES on that backend, the provider (and anyone who breaches it, and any subpoena served on it) holds ciphertext. Someone reading the raw bucket sees a `DGE1` magic header, an IV, and length-prefixed encrypted chunks. The dumps are readable only through Acme's proxy. Crucially, delta compression runs *before* encryption, so the storage savings on those highly-similar nightly dumps are fully preserved — ciphertext of a delta is no bigger than the delta.

With `sse-kms`, the proxy delegates encryption to AWS and never touches key material; every write carries the SSE headers and AWS does the rest. What you're buying is AWS's key-management story: per-key IAM, automated rotation, and a CloudTrail event for every single decrypt. If an auditor asks "who read these objects in March," SSE-KMS can answer; proxy-AES cannot (the key never moves, so there's no per-decrypt event — only the proxy's own access logs).

`sse-s3` is the budget cut of the same idea: AWS-managed AES-256, no KMS cost, no per-decrypt audit trail. Encrypted on AWS's disks, transparently decrypted for any authorized IAM caller.

And `none` is a legitimate choice, not a default you forgot to change. Acme's `downloads` bucket serves public installers from its `public/` prefix — encrypting world-readable artifacts is pure CPU overhead with zero threat-model benefit.

The decision compresses to two questions. **Is the backend storage untrusted (third-party provider, hostile jurisdiction, compliance says "provider must not see plaintext")?** Then proxy-AES — it's also the only encrypting option for filesystem backends like `local-disk`, since the native modes are S3-only. **Is the backend AWS and your compliance story AWS-native?** Then SSE-KMS if you need the audit trail and key lifecycle, SSE-S3 if "encrypted at rest: yes" on a checklist is the actual requirement. Acme runs all three answers at once: proxy-AES on `hetzner-fsn1`, SSE-KMS on `aws-dr`, nothing on the public-CDN path — encryption is per-backend precisely so one proxy can hold all of these postures simultaneously.

## Why enabling isn't retroactive

Flipping a backend to `aes256-gcm-proxy` encrypts *new writes only*. Existing objects stay in their stored form. This surprises people, so it's worth explaining as a design choice rather than a gap.

Reads dispatch on a per-object metadata marker (`dg-encrypted`), not on the backend's current mode. An object without the marker is served as-is; an object with it is decrypted. This is what makes enabling encryption safe and instant: nothing breaks at flip time, no migration is forced on you, mixed plaintext/ciphertext backends just work. The alternative — rewriting every existing object synchronously at config-apply time — would turn a config change into an unbounded, failure-prone bulk operation hidden inside an apply button.

Instead, bringing history under the new mode is an explicit, visible operation: the **re-encrypt job**, a durable one-off that rewrites every object whose markers don't match the backend's configured mode, resumable across restarts, with writes to the bucket gated while it runs. The Backends page proposes one whenever you change encryption settings. See [how to encrypt data at rest](../how-to/encrypt-data-at-rest.md) for the procedure.

## Why in-place rotation is unsupported — and the shim that designs around it

You cannot just change the `key` on a proxy-AES backend. Do that, and every object written under the old key becomes unreadable — AES-GCM doesn't degrade gracefully, it authenticates or it fails. Supporting transparent multi-key rotation would mean a key ring, per-object key resolution against an unbounded set, and a quiet pile of complexity in the hottest read path. The proxy refuses that trade.

What it offers instead is deliberately minimal: a **decrypt-only `legacy_key` shim** holding exactly one previous key generation. Every proxy-AES write stamps a `dg-encryption-key-id` on the object; reads check the stamped id against the current key, then against the legacy slot. Writes never use the legacy key — so during a rotation, old objects stay readable while everything new lands under the new key, and the population converges in one direction only. Run the re-encrypt job to rewrite the stragglers, clear the shim, done. One legacy slot is a constraint, but it's also a forcing function: rotations finish, rather than accreting into a museum of key generations. The key-id mechanism also buys honest errors — a mismatch tells you *which* key an object wanted, instead of an opaque GCM authentication failure. The full procedure is in [how to rotate encryption keys](../how-to/rotate-encryption-keys.md).

The one warning that deserves bold text: **key loss in proxy-AES mode is data loss.** The proxy does not escrow keys; there is no recovery path. Back the key up off-box — secrets manager, vault, sealed envelope — before the first encrypted write.

## Why metadata stays plaintext

Under every mode — including SSE-KMS — object names, sizes, content-type, and `x-amz-meta-*` user metadata are stored unencrypted. For the native modes this is an AWS constraint: SSE encrypts bodies, full stop. For proxy-AES the proxy mirrors that policy, partly for consistency, and partly because of a chicken-and-egg problem: the metadata *is* how the read path detects whether an object is encrypted at all. Encrypting the marker that says "this is encrypted" doesn't work.

The honest consequence: an attacker with backend access learns names, approximate sizes (ciphertext length tracks plaintext length), and anything you put in metadata. If a value is secret, it belongs in the object body.

## The honest costs

Proxy-AES is not free, and the costs are concrete. **CPU and latency:** AES-256-GCM with hardware acceleration runs at roughly 1–3 GB/s per core; a 100 MiB upload adds ~30–100 ms of proxy-side crypto. **Memory:** encrypted reads stream (≈130 KiB in flight regardless of object size), but encrypted *writes* buffer the encrypted frames before handoff — a 100 MiB upload can peak around 200–300 MiB RSS once multipart buffering stacks on top. Size your proxy accordingly, or pick a native mode, which moves all of this to AWS. **Disk:** a large upload or a multipart upload is stored from a file, and the proxy first encrypts that file into a temporary file in the spool directory (`DGP_SPOOL_DIR`). The temporary file counts against the spool budget (`DGP_SPOOL_MAX_BYTES`), in the same way as the files of the delta codec, so encrypted writes cannot fill the disk. When other requests use the whole budget, the proxy answers the upload with `503 SlowDown`, and S3 clients retry it. One cost you might expect but don't pay: range requests still work on encrypted objects — the chunked format locates the needed 64 KiB chunks in O(1), decrypts only those, and trims. The overhead is at most one chunk of waste at each end.

## Who can read what, outside the proxy

The boundaries above have a practical corollary: the proxy must be the write and read path for encrypted backends. The original Python DeltaGlider CLI speaks the same delta format but does **not** encrypt — pointed at raw storage instead of the proxy, it writes plaintext into a bucket you believed was encrypted. Conversely, raw `aws s3 cp` against an SSE-KMS bucket returns plaintext to any authorized caller, because that's what SSE-KMS *means*. Neither is a bug; both are reminders that the mode you picked defines exactly who can bypass the proxy and what they see. If the answer must be "nobody and nothing," that's proxy-AES, with all clients pointed at the proxy.

## Related

- How-to: [Encrypt data at rest](../how-to/encrypt-data-at-rest.md)
- How-to: [Rotate or change encryption keys](../how-to/rotate-encryption-keys.md)
- Reference: [Encryption](../reference/encryption.md)
