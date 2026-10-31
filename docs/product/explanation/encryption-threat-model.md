# About the encryption threat model

This page describes what proxy-side encryption (`aes256-gcm-proxy`) protects, and what it does not protect, against three kinds of attackers. It describes the format that the proxy writes from this release on: per-object keys, authenticated frames, and sealed metadata. The mechanism of each part is in the [encryption reference](../reference/encryption.md#format-v2-proxy-aes). Why you would choose proxy-side encryption at all is in [About encryption at rest](encryption-at-rest.md).

The three attackers are:

- **Eve** can read everything on the backend, but she cannot change it. A breached storage provider, a leaked bucket, a stolen disk and a subpoena served on the provider are examples.
- **Dropbox** can read and write everything on the backend. A storage provider that turns hostile, or an attacker who holds the backend credentials, are examples.
- **Mallory** is a legitimate user of the proxy. She signs her requests with her own credentials, and her IAM policy lets her read some objects and not others.

All three attackers work from outside the proxy. The proxy host, its memory, its configuration file and its keys are trusted. An attacker who controls the proxy host reads every key, and no storage format can protect against that.

The example deployment stores the bucket `db-archive` on the backend `hetzner-fsn1`, with its own proxy key. `backup-bot` writes nightly database dumps into it, and `dana` may read only the folder `db-archive/reports/`.

## Eve: an attacker who reads the backend

Eve reads the stored bytes and the stored metadata of every object, but she has no key.

**What is protected.** The content of every object is encrypted with AES-256-GCM under a key of that object alone. This applies to objects stored as they are, to deltas and to the delta references of every folder. The hashes of the content are sealed, too: the backend holds no MD5, no SHA-256 of an object, and no SHA-256 of a delta's reference. Eve therefore cannot confirm a guess about a file by its hash. The listing facts of an object (its size and its ETag) are encrypted in its listing token, and the size of the object behind a delta is sealed.

**What is not protected.**

- **Names.** The names of objects and folders stay in clear, because S3 lists and sorts by name. A name such as `db-archive/nightly/customers-2026-10-14.dump` tells Eve what the object holds. The content type, the user metadata, the original file name, the creation time and the storage type (delta, reference or passthrough) also stay readable.
- **Exact sizes.** Every object uses the same framing, so the stored size of an object that the proxy stores as it is gives its exact plaintext size. The same is true for a delta reference. The stored size of a delta gives the exact size of the delta, but not the size of the object.
- **Delta similarity.** A delta is encoded against the reference of its folder, so a small delta means that the new object is close to that reference. Eve sees the size of every delta. She therefore learns which uploads are almost the same as earlier ones, for example that two nightly dumps differ in only a few rows. When Eve can also cause uploads into the folder, she can test guesses with this oracle (see Mallory below).
- **Which objects share a key.** Every object stores the fingerprint of its key. Eve sees which objects, in which buckets, use the same key. When two backends use the same key, Eve can also link their objects.
- **Activity.** The provider sees every request that the proxy sends: which objects the proxy reads and writes, how often, and when.

## Dropbox: an attacker who reads and writes the backend

Dropbox learns everything that Eve learns. Dropbox can also change, add, copy and delete stored objects.

**What is detected.** The proxy refuses to serve an object that Dropbox changed, with `500 InternalError` and a log line that names the cause. These changes are detected:

- A changed, cut, extended or reordered body. Every frame is authenticated, and the last frame carries a final flag, so a body that ends early fails even at a frame boundary.
- A body or a whole object moved to another name, another folder or another bucket, and two objects swapped. The sealed metadata is bound to the real bucket name, the folder and the name of the object. The header of the body is bound to its sealed metadata by a random salt.
- A changed fact in the sealed metadata, such as the size or the ETag.
- A changed content type or changed user metadata of the client. The seal holds a digest of these two clear fields. A changed `Content-Type` therefore cannot make a browser run a stored file as a web page.
- A removed seal, a removed encryption marker on an encrypted body, or a body under an unknown key.
- A planted plaintext object. A bucket that encrypts refuses every object without encryption, except the plaintext history that a conversion window serves. The window serves only objects that the backend stored before the cutoff of the conversion, so an object that Dropbox plants later is refused (see [The conversion window](#the-conversion-window)).
- Forged listing facts. In a backend that holds a key, the proxy ignores the listing facts of the older form, and a listing token that does not open is ignored.

**What is not detected.**

- **Rollback to an older version.** Dropbox can keep a copy of an object and its metadata, and later put that copy back at the same name. The old copy has a valid seal for that location, so the proxy serves the older content as if it were current. The proxy keeps no record of the latest version of each object, because such a record would need durable shared state and would break a restore from backup. There is one exception: the sealed metadata of a delta holds the SHA-256 of the reference that the delta was encoded against. When Dropbox rolls back a reference under deltas that need the newer reference, the reconstruction of those deltas fails. When Dropbox rolls back a whole folder, deltas and reference together, the proxy detects nothing.
- **Replay between two backends.** The name of the backend is not part of the seal, so that a renamed backend keeps its data. Two backends that share a key and a real bucket name therefore accept each other's objects. When `hetzner-fsn1` and another backend both store a bucket named `db-archive` under the same key, Dropbox can copy an object from one to the other, and the proxy serves it there. Give each backend its own key to close this.
- **Plaintext that Dropbox plants before the cutoff, or within the grace after it.** While a conversion window is open, the proxy serves every object without encryption that the backend stored before the cutoff plus 300 seconds, and the re-encrypt job seals it as part of the bucket's history. An object that Dropbox planted before that time is therefore sealed like the real history, and nothing can tell them apart afterwards. See [The conversion window](#the-conversion-window).
- **Deletion.** Dropbox can delete any object. When Dropbox deletes the reference of a folder, the deltas of that folder can no longer be rebuilt. Encryption protects confidentiality and integrity, not availability. Keep backups of the backend.
- **Changed proxy fields in clear.** The proxy also keeps some of its own fields in clear: the original name, the tool, the note and the reference path. The digest in the seal covers only the content type and the client's user metadata, so Dropbox can change these other fields, and the proxy shows the changed values in its responses. The body and the sealed facts are not affected.
- **A removed or replaced key claim.** A proxy that encrypts a bucket keeps a key claim in it, so that a second proxy without the key, or with another key, does not write the bucket (see [Buckets that another proxy encrypts](../reference/encryption.md#buckets-that-another-proxy-encrypts)). Dropbox can delete or replace the claim. The proxy then still finds the key of the objects from a sample of the bucket and from the objects that it reads, but a bucket whose encrypted objects are outside the sample can be written by a second proxy again. The claim protects against a misconfigured proxy, not against Dropbox.
- **Removed listing tokens.** When Dropbox deletes a listing token, a LIST request computes the size of an object that is stored as it is from its stored size, and it reads the metadata of a few deltas per page. The ETag of such an object, and the size of a delta beyond those few, are wrong in that one listing, until the proxy writes the token again in the background. A GET or a HEAD request is not affected. See [Listing tokens](../reference/encryption.md#listing-tokens-s3-backends).

**Native SSE gives no authenticity.** Under `sse-kms` and `sse-s3`, AWS encrypts the stored bytes and decrypts them for every request that the backend credentials sign. The proxy keeps no seal for these objects. An attacker who can write to the bucket through S3 can therefore replace any object, and the proxy serves the replacement. The native modes protect against Eve, but not against Dropbox.

## Mallory: a legitimate user with limited permissions

Mallory reads, and perhaps writes, some objects through the proxy. In the example, `dana` reads only `db-archive/reports/`.

**What is protected.**

- **Other users' objects.** IAM decides what Mallory reads, and the storage format does not change that. Mallory knows the plaintext of every object that she may read. Every object has its own keys, so that knowledge tells her nothing about the keys or the content of other objects.
- **The delta oracle through the proxy.** The storage facts of a write and of a read (the storage type, the stored size, the reference SHA-256, and whether the write created a baseline) go only to a principal that may read every object in the folder. When Mallory may read or write only some keys of a folder, the proxy gives her none of these facts. She therefore cannot upload a guess and read from the response how close it is to another user's object. The rule is in [Storage facts on write responses](../reference/s3-api-compatibility.md#storage-facts-on-write-responses).
- **Copies.** A copy through the proxy (CopyObject, replication, migrate or a lifecycle transition) reads the source with the permissions of the request, and the proxy writes new sealed metadata for the destination. Mallory can only copy what she may read.

**What is not protected.**

- **The delta oracle when Mallory can also read the backend.** When Mallory may write into a folder and can also see the backend as Eve does, she can upload a guess through the proxy and read the size of the stored delta on the backend. A small delta confirms that her guess is close to the reference of the folder, which can hold another user's data. The proxy cannot hide the size of a stored delta from the backend. To keep two groups of users apart, give them separate folders or buckets, so that their objects never share a reference, or turn off compression for the bucket.

## The conversion window

A bucket that starts to encrypt usually holds objects already, and these objects are plaintext. The proxy must serve this plaintext history while the re-encrypt job converts it, but it must not serve plaintext that Dropbox writes into the bucket during that time. The conversion window is how the proxy tells the two apart.

**How the window opens.** When a config write makes a bucket that holds objects start to encrypt with a proxy key, the proxy writes a conversion marker into the bucket. The marker is one small internal object: `.dg/facts/!!conversion` on an S3 backend, and the file `.dg-conversion` in the bucket directory on a filesystem backend. The marker holds three facts: the cutoff, the deadline and a random run id. The cutoff is the time of the backend's own clock when the window opened. The deadline is 7 days after the cutoff. The proxy seals these facts with AES-256-GCM-SIV, under a key that it derives from the bucket's encryption key. The seal is bound to the storage identity of the bucket, which is the backend and the real bucket name. The proxy then queues a re-encrypt job for the bucket.

**What the window serves to clients.** A client request (GET, HEAD, a range read or the source of a copy) reads an object without encryption only when both of these are true:

- The window is open: the marker opens with a key of the bucket, it names this bucket on this backend, and its deadline is still ahead on the backend's clock.
- The backend stored the object before the cutoff plus a grace of 300 seconds. The proxy takes this time from the backend's own record of the last write: the `Last-Modified` time on S3, and the modification time of the file on a filesystem backend. An S3 server sets `Last-Modified` itself on every write, so a writer cannot choose it. The proxy never uses its own `dg-created-at` field here, because a writer chooses that value. When the backend names no time for the object, the proxy refuses it.

The grace covers small differences between the clock that set the cutoff and the clock that stamps the objects, and uploads that started before the cutoff and that the backend stored a little after it. The price of the grace is that Dropbox has 300 seconds after the cutoff in which a planted object is still served as history.

Every other object without encryption is refused with `500 InternalError`. That includes an object that Dropbox writes after the cutoff, and also a plaintext overwrite of an old object, because the overwrite gets a new `Last-Modified` time.

**What the job reads.** The re-encrypt job must convert every object, so it reads every object without encryption in the bucket, whatever its stored time. An object that a client could not read (stored at or after the cutoff plus the grace, or with no stored time) is converted too, but the job reports it as *adopted after the cutoff*. The job lists each such object on the **Failures** tab of its run as information, writes an audit entry for it, names the count in its run summary, and counts it in the metric `deltaglider_conversion_adopted_after_cutoff_total`. Check every object on that list: one of them can be an object that Dropbox planted, and after the job it carries a valid seal like the rest of the bucket.

**How the window closes.** When the job converted every object, it closes the window: it removes the marker, but only when the marker still holds the run id of its own conversion. When some objects failed, the window stays open, and **Resume** starts the job again. When the deadline passes before the job finishes, the window closes for clients, and the plaintext objects that are left fail to read until you resume. A resume sets a new deadline, 7 days from then, and it keeps the original cutoff. A resume therefore never serves an object that the backend stored after the first cutoff.

**What Dropbox can do to the window.**

- **Close it.** Dropbox can delete the marker or change its bytes. A marker that does not open counts as a closed window. Clients then get errors for the plaintext history until the job converts it. The job still converts the bucket. When the marker is still there but does not open, the job does not remove it, and its run ends with an error that names the marker. This costs availability, but no plaintext is served.
- **Not open one.** Dropbox does not have the key, so it cannot write a marker that opens. A marker copied from another bucket, or from a bucket with the same name on another backend, does not open either, because the seal names the storage identity.
- **Replay an old marker.** Dropbox can keep the marker of a conversion and put it back after the job closed the window. The replayed marker counts only until its deadline, and it serves only objects that the backend stored before its cutoff. The job converted those objects already. A plaintext copy that Dropbox writes back over a converted object gets a new `Last-Modified` time, so the replayed window does not serve it.

**Honest limits.**

- **Instances that do not encrypt the bucket yet.** The marker narrows reads only on instances whose configuration already encrypts the bucket. An instance that still runs the old configuration writes plaintext into the bucket and reads plaintext from it. With several instances, apply the encryption on every instance first, and then let the job finish. An object that a lagging instance writes after the cutoff is refused for clients and reported as adopted after the cutoff.
- **Versioned backends.** On a backend that keeps old versions of objects (versioning enabled or suspended), the job converts the current objects, and their plaintext versions stay on the backend as noncurrent versions. Anyone who can read the backend can read them. The job can delete them when you ask for it (**Also delete the old unencrypted versions** in the admin UI, or `purge_plaintext_versions` in the admin API). That deletion cannot be undone.
- **The filesystem backend.** The modification time of a file is not a server clock. A local user who can write the bucket directory can set it, for example with `touch -d`, and `cp -a` keeps the time of the source file. On a filesystem backend, the cutoff therefore stops only writers that cannot change file times. A filesystem backend is local to one host, so such a writer already has access to the proxy host.

## Copies of facts on the proxy host

The proxy host keeps some facts that the backend no longer holds in clear. The node-local jobs database (`deltaglider_jobs.db`) caches, for replication parity and for lifecycle scans, the ETag, the size and, for parity, the SHA-256 of objects. That database is encrypted with the config DB key, and it never leaves the host. It is part of the trusted proxy host, so an attacker with access to the host and to the config DB key can read these facts, together with every encryption key.

## Plaintext in the spool directory

The delta codec (xdelta3) works on plaintext, so the proxy decrypts an object before it computes or applies a delta. For an object larger than the spool threshold (16 MiB by default), it writes the plaintext to files in the spool directory (`DGP_SPOOL_DIR`): the reference of the folder, the new object, and the rebuilt object of a delta GET. The relay parts of a multipart upload and the cache of rebuilt objects for range requests are plaintext files in the same directory. The proxy removes each file when its request ends, and startup removes the files that a crash left behind. These files are part of the trusted proxy host, but they are on a disk, not only in memory. Put the spool directory on a local disk of the proxy host that no backup copies and that no other host mounts, because a copy of that disk holds plaintext of encrypted objects.

## A copy outside the proxy

The seal binds an object to its location. A copy that the backend makes on its own, such as an `rclone` copy to another bucket name or a restore of a backup under another bucket name, therefore does not read through the proxy. This is the same check that stops Dropbox from moving objects, and the proxy cannot tell an operator's copy from an attack. Copy encrypted objects through the proxy, or restore them under the same bucket names and keys.

## Related

- [About encryption at rest](encryption-at-rest.md): the choice of mode, the keyring, and why enabling is not retroactive.
- [Encryption reference](../reference/encryption.md): the format, the markers, the read rule and the limits.
- [About authentication and access control](security-model.md): authentication, IAM, and the infrastructure secrets of the proxy.
