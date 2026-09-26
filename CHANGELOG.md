# Changelog

## Unreleased

### Fixed — A lockout says that it is a lockout, and for how long

A locked-out sign-in got a bare `429` (or `{"ok": false}`), so the UI showed
"Login failed: Login failed", even for the correct password. Now every
admin sign-in surface answers `429` with `Retry-After` and
`{"error": "too_many_attempts", "message": "… Try again in N min.",
"retry_after_secs": N}`; the S3 API keeps `503 SlowDown` and adds
`Retry-After` and the wait to the message.

### Changed — The bootstrap access key id is visible, and its removal is explicit

The Credentials page showed an empty access key id field, because every GET
and export redacted it, and it said "to remove them, clear both fields".
Clearing both fields with no IAM users turned S3 authentication off at
runtime. Now the access key id (an identifier, not a secret) is shown; the
secret stays redacted, and an unchanged key id with no secret keeps the
secret on apply. `DELETE /_/api/admin/config/bootstrap-credentials` removes
the pair; it and every other config change refuse to remove it while no IAM
users exist. With IAM users, the pair becomes the fallback for an empty IAM
DB instead of an ignored edit.

### Added — OIDC providers in private networks

An identity provider on a private address or behind a private CA (an
in-house Keycloak or Dex) could not be used: discovery refused the address,
and the proxy trusted only public roots. Now a provider's `extra_config`
takes `allow_local: true` (http:// and private addresses allowed, cloud
metadata never, as for backends and webhooks) and `ca_cert_path` (a PEM
bundle added to the trust roots). The admin API and the declarative
reconciler check the issuer URL against that policy at save time (422 with
the reason), and the admin API checks the CA file. Create and update
responses no longer echo `client_secret`. Discovery and token errors keep
their cause (for example `UnknownIssuer`). `set-up-sso.md` no longer
suggests `curl` as a check of what the proxy can reach.

### Fixed — IAM users count as credentials at boot

A declarative config whose only credentials were `access.iam_users`, and a
GUI-mode proxy whose config DB held IAM users but whose config had no
bootstrap SigV4 pair, both refused to start with "No authentication
configured". Now the startup check runs after the config DB is open and
counts declarative `iam_users` and existing IAM DB users as configured
credentials. The fatal help text names these options.

### Security — S3 clients cannot reach the coordination bucket

The `config_sync_bucket` holds the synced IAM database, the replication
leases and the reference locks, but any client with write access to it
through the proxy could replace them (for example, put back an old IAM
copy that re-enables a disabled key on every instance). Now the proxy
refuses every S3 request to that bucket with `403`, for every identity,
hides it from ListBuckets and the admin bucket list, and refuses it in the
admin bulk endpoints. A config that makes it public, aliases onto it, or
uses it in a replication or lifecycle rule is refused. As a second line of
defence, the sync refuses a downloaded copy with rows older than the last
synced copy.

### Fixed — Session cookies are `Secure` when TLS comes from the YAML file

The `Secure` flag on admin session cookies followed only the
`DGP_TLS_ENABLED` variable, so a listener with `advanced.tls.enabled: true`
in the YAML file sent cookies without it. Now the flag follows the TLS state
of the running listener. The docs no longer claim that `DGP_SECURE_COOKIES`
defaults to `true`: unset means automatic.

### Fixed — TLS no longer panics on the first HTTPS request

With `tls.enabled: true` the proxy panicked on the first HTTPS handshake
(self-signed) or at startup (user PEM): the dependencies enable two rustls
crypto providers, and rustls refuses to pick one. Now the proxy installs
aws-lc-rs as the process default before anything builds a TLS config. An
integration test boots the listener in both modes and sends HTTPS requests.

### Changed — The admission chain holds at most 1000 blocks

The proxy checks the admission blocks for every request, and the chain had
no size limit (a 20000-block config was accepted). Now a config with more
than 1000 operator blocks is refused with an error that names the count and
the limit, on load and on every admin apply.

### Changed — A refused reserved key says which rule refused it

A `PUT` of a key such as `docs/reference.bin` or `backups/db.sql.delta` got
"reserved for internal use" with no reason. Now the message names the key,
the rule and the reason (the proxy stores the delta baseline and the
delta-encoded objects under these names), and links the new "Reserved and
normalised keys" section of the S3 API compatibility reference. That section
also lists the keys that the proxy changes or refuses and that S3 stores
unchanged: a leading `/` is removed, and empty (`//`) or `..` segments are
refused.

### Changed — The `s3` CLI verbs refuse a DeltaGlider Proxy endpoint

The `s3` verbs run their own delta engine and write the storage layout
themselves, so they must talk to the storage backend. Pointed at the proxy,
they failed with an unclear `400` on the internal key names. Now each engine
verb (`ls`, `cp`, `rm`, `sync`, `stats`, `verify`, `migrate`) first sends one
unauthenticated `GET /_/health` to `--endpoint-url`. When the endpoint is a
DeltaGlider Proxy, the verb stops with exit code `2` and says to use the
backend's endpoint, or a plain S3 client for the proxy.

### Fixed — An `allow-anonymous` request rule lets the matched read through

An operator-authored `allow-anonymous` rule (for example, the documented
`allow-public-zips` rule) marked the request as anonymous, but the
`$anonymous` principal got its permissions only from the bucket's
`public_prefixes`. On a bucket without public prefixes, authorization then
refused the request with `403`, while the trace said `allow-anonymous`.
Now the rule grants exactly the request that matched, when it is a read: a
`GET` or `HEAD` of that object, or a listing of that bucket with that
prefix. Writes are never granted. One function decides the grant for the
live path and for the trace, which reports it as `anonymous_grant`.

### Fixed — A one-off job with failed objects no longer shows "succeeded"

A re-encrypt or backfill job records a failed object and goes on to the
next one. At the end the job settled as `completed` (shown as `succeeded`)
even when every object failed. Now such a job settles as
`completed_with_errors` when some objects went through, and as `failed`
when none did; `last_error` names the failure count. A migrate whose
source cleanup fails to delete some objects is also `completed_with_errors`.
On the Jobs screen the row shows "N failed", which opens the Failures tab.

### Fixed — A migrate cancel stops within 20 objects and removes its copies

A migrate job checked for a cancel only once per 1000-object page, so a
cancel waited for the page to end, and the cancelled job left its copies on
the destination. Now the job checks every 20 objects and saves its progress
at the same points. A cancel or a failure before the routing flip releases
the source bucket's write gate at once, then deletes the copies that this
job wrote to the destination (found by the job id in their `dg-migration`
metadata; other objects stay), then removes the staging route.

### Fixed — Moving a bucket back no longer brings deleted objects back

A migrate job copied the source on top of whatever the destination bucket
already held. After a move from A to B that kept the safety copy on A, an
object deleted on B came back when the bucket moved back to A. Now a migrate
refuses a destination that already holds objects: the job fails in its
`stage` phase, before any copy, and the error names the object count. The new
`"target": "mirror"` option (the "exact mirror" checkbox in the migrate
dialog) makes the destination an exact copy instead: destination objects that
the source does not hold are deleted before the flip, and each delete is
audited as `maintenance_migrate_mirror_delete`.
### Fixed — Small drift in the admin UI, the docs and the logs

- Docs search: a result no longer shows the changelog's "GENERATED FILE"
  comment, and a search for an identifier in backticks (for example
  `replication_target_only`) finds it and shows it in the snippet.
- An unknown page under `/_/` (for example a mistyped `/_/setings`) shows a
  404 page in the UI theme with a link home, not a bare "not found". Missing
  files (assets, source maps) still get the plain 404.
- An unknown settings page no longer marks Dashboard as the current page in
  the sidebar.
- The re-encrypt proposal no longer says that rewritten objects get a new
  Last-Modified. The job keeps each object's Last-Modified and ETag.
- The docs no longer call `/_/stats` unauthenticated. It answers only an
  admin session (`401` otherwise); the examples now sign in first.
- The Jobs screen can start the metadata-backfill job: **New job → Backfill
  metadata…**. The job and its API are documented in the jobs reference.
- A HEAD that finds no bucket or no object on an S3 backend (a routing or
  existence probe) logs at debug. It used to log a WARN "S3 error" line.
- Sortable table headers (object browser, event outbox) show a focus ring in
  both themes, and Space sorts as well as Enter.

### Fixed — The Credentials page shows the bootstrap access key and removes it on request

The page showed an empty access key field while a bootstrap key was
configured, and it said "to remove them, clear both fields", although an
empty field keeps the current value. The field now shows the configured
access key ID, and a **Remove bootstrap credentials** button removes the pair
after a confirmation.

### Fixed — A locked-out sign-in says that it is locked, and for how long

After too many failed sign-ins the proxy refuses every attempt from that
address for a while, including one with the right password. The sign-in
forms showed "Login failed: Login failed", so the operator kept retrying.
Every sign-in form (admin password, IAM keys, re-login, browser session) now
shows "Too many sign-in attempts. Try again in N min." and reads the wait
from `Retry-After` (or `retry_after_secs` in the JSON body) when the server
sends it. A wrong password shows "Login failed: wrong password." once.

### Fixed — The upload page keeps up with many small files

Every upload progress event rebuilt the whole queue and re-rendered every
row, so the cost of a batch grew with the square of its size: 300 small
files took about nine minutes. Progress now goes to a keyed store and
reaches the page at most once every 50 ms, the next file starts as soon as
the previous one ends (not after a render), and the queue list renders only
the rows in view. 1000 small files upload at the speed of the server, with a
bounded page memory.

### Fixed — An encryption change on the Backends page keeps the other backends intact

The Backends page built the `backends` list for its section PUT from the
backend summaries, which carry only a few fields. Because the PUT replaces the
whole list, an encryption change on one backend removed `allow_local` and the
`encryption` block from every other backend: an `http://` S3 backend was
refused, and a proxy-AES sibling was switched to `mode: none`. The page now
reads the storage section, copies every entry as it is, and changes only the
target's `encryption`. The PUT carries `If-Match`, so a concurrent edit is
refused instead of overwritten.

### Fixed — Rotate key no longer makes existing objects unreadable

The **Rotate key** button sent only the new key, because the admin UI never
sees the current one. The server then dropped the current key, so every
object written under it answered `500` and the re-encrypt job failed on every
object. Now a config apply that changes a proxy-AES key (or moves a backend
away from `aes256-gcm-proxy`) and does not set `legacy_key` keeps the previous
key as `legacy_key`, together with the key id that the old objects carry. A
mode change used to keep the key without its id, so the shim matched no
object; it now keeps the id too. The apply is refused when the one legacy slot
already holds a different key, or when the new key keeps the old `key_id`.
Send `legacy_key: null` to drop the previous key on purpose.

### Changed — Declared buckets are created at boot on S3 backends too

A bucket declared under `storage.buckets` was created at boot only on a
filesystem backend; on an S3 backend its first write failed with
`NoSuchBucket`. Now the proxy creates a missing declared bucket on either
kind of backend (on S3: `HeadBucket`, then `CreateBucket` only when it is
missing; a failure only warns). `DGP_BOOT_CREATE_DECLARED_BUCKETS=false`
turns this off.

### Fixed — The admin UI loads compressed, and the file browser skips the docs stack

The UI's HTML, JS and CSS went out uncompressed (several MB on a first
load), and the file-browser page preloaded the 330 kB markdown stack that
only the docs need. The UI routes (and `GET /_/api/docs`) now answer gzip or
br when the browser accepts it; S3 responses are not touched. The markdown
stack moved to the lazy docs chunks, and `check-bundle-fingerprints.sh`
fails the build if `index.html` loads it up front again.

### Fixed — Bucket migration and re-encryption keep each object's time

A migrated or re-encrypted object got the time of the copy as its
`dg-created-at` (served as `LastModified`). After a migration every object
looked new, lifecycle ages started again, and newer-wins compared the copy
times. Now both jobs stamp the destination with the source object's
created-at, on every copy path (buffered, delta, spooled, streamed).
`reference.bin` baselines keep the real time.

### Changed — Bulk copy, move, delete and ZIP work for users without admin rights

The bulk actions in the file browser needed an administrator session, so a
user who signed in with an access key could select files but not act on
them. Now the bulk endpoints (`/_/api/admin/objects/*`) also accept that
browser session, and the proxy checks every key against the user's own IAM
permissions with the S3 API's rules (`aws:SourceIp` included). A key that
the user may not use is reported per key with `AccessDenied`; the others are
processed. A folder listing returns only the keys that the user can see.

### Fixed — Two tabs editing one config section no longer overwrite each other

The config endpoints were last-writer-wins: a second browser tab applied its
stale copy of a section over the first tab's edit, with no warning. Now the
section GET, `GET /config` and the export return a version in `ETag`; the
section PUT, `PUT /config` and `POST /config/apply` accept `If-Match`, and a
stale one gets `409 Conflict` with the current version. The admin GUI sends
`If-Match` on every section apply, and on a conflict it keeps the edits and
offers to reload or to review them against the new version. A write without
`If-Match` is not checked.

### Fixed — Webhook delivery: local receivers, fast dead letters, a Failing state

A webhook URL on `http://` or on a private address applied with only a
warning, and then every event retried eight times and failed. Now:

- `event_delivery.allow_local: true` allows `http://` and private or
  loopback addresses for the webhook URLs (cloud metadata stays refused),
  like a backend's `allow_local`. The admin UI has an **Allow local
  receivers** switch.
- A config apply that adds a URL that the policy refuses is refused. A URL
  that the config already held only warns, so an upgrade still boots.
- An error that no retry can fix (a refused or invalid URL, an invalid
  header, no target) fails the row after one attempt. Its error starts with
  `[permanent]`.
- The event-outbox API returns `delivery_state` (`failing` when the newest
  delivery failed) and `last_delivery_error`; the admin UI shows **Failing**
  instead of **Active**.
- The redaction of URLs in error text no longer turns the advice "(use
  https://)" into "(use <invalid-url>)".

### Fixed — A hung backend no longer hangs requests, and the proxy notices it

The health probe re-checked only backends that were already unhealthy, so a
backend that hung after boot stayed "Connected", and requests to it waited
for minutes (a 60 s read timeout, three times). Now every backend is probed
every 30 s (`DGP_BACKEND_HEALTH_INTERVAL_SECS`). A backend request without a
large body has a 30 s deadline, retries included
(`DGP_BACKEND_REQUEST_TIMEOUT_SECS`); when it passes, or the connection
fails, the client gets `503 ServiceUnavailable` naming the backend (before:
`500 InternalError`), and the backend is marked unhealthy at once, so the
next requests get the gate's 503 without a wait. Uploads and server-side
copies keep their old timeouts. `GET /_/ready` now lists each backend's live
state in `backends`, and reports not-ready when every backend is down.

### Changed — A backup restore replaces users and groups (point-in-time)

A restore of `iam.json` added the users and groups that the instance did not
have, and it kept everything else. A user that someone created or changed
after the backup survived the restore, so a restore was not a return to the
backup's state. Now a restore replaces the users, groups, OIDC providers,
mapping rules and external identities with the backup's, in one database
transaction: a failure part-way leaves the database as it was. Add
`iam=merge` to `POST /_/api/admin/backup` (or pick "Merge" in the restore
dialog) to get the old behaviour. The result reports `users_deleted` and
`groups_deleted`.

### Fixed — Admin UI and object browser, from a browser review

- **A new or duplicated user's secret is shown.** Create and Duplicate
  select the new user, and the URL change cleared the one-time secret
  before anyone saw it. The credentials now stay in a dialog, with copy
  buttons, until the admin presses "I have copied it".
- **Lifecycle Run now asks first.** It shows the preview (the keys, their
  count and bytes) and runs only on the button that names the count.
  Preview is a tab in the job drawer, not a toast.
- **Upload.** The page opens at the current folder, even a read-only one:
  it says "You cannot upload to …" and offers the writable prefixes
  instead of moving the destination by itself. For admin sessions it
  refuses a file over `max_object_size` or the remaining quota before the
  upload starts. The Buckets page edits `max_object_size`.
- **Download** streams through the browser from a short-lived presigned
  URL, in one click, instead of holding the whole object in page memory.
- **Numbers.** The analytics hero shows the saved share (never more than
  100% smaller), and the cache "avg per entry" divides the used bytes.
  A passthrough file says it is stored as-is, not that compression is off.
- **Sessions.** A session that expires mid-edit asks to sign in again in a
  dialog, keeps the edits and retries the step. A non-admin in Settings
  reads that the account has no admin rights. The "files only" tip is
  remembered per user and is one line on a phone.
- **Navigation.** An unknown admin link shows a not-found page with the
  nearest real page. A `?object=` link into another bucket sends its first
  request to that bucket. Double-click on a file opens the preview.
  The palette's Show YAML and Apply YAML open the YAML dialog.
- **First run.** The empty browser links to the setup wizard, the wizard
  keeps its apply warnings on screen, and the fatal no-authentication
  message shows YAML. The startup banner names every backend.
- **Accessibility.** Text and primary buttons reach 4.5:1 contrast in both
  themes; one `<main>` and one `<h1>` per view; no button inside a button;
  named progress bars and row buttons; clickable pills are buttons; every
  confirmation is a dialog instead of `window.confirm`; focus returns to
  where it was when an overlay closes.
- **Polish.** Bucket-name inputs explain an invalid name instead of
  rewriting it; the theme follows `prefers-color-scheme` until you pick
  one; the Apply dialog disables Apply when there is nothing to apply;
  the Credentials page no longer says the admin password encrypts the IAM
  database. An admin UI apply still rewrites the config file in canonical
  form; the configuration reference now says so.

### Fixed — Leases and locks read a racing conditional write as a lost race

With config sync on, the replication lease, the reference lock and the backend
CAS probe make conditional writes to S3. AWS answers two conditional writes
that race each other with `409 ConditionalRequestConflict`. These paths
checked only for `412 PreconditionFailed`, so a 409 made a lease or lock step
fail with an error instead of reporting that another instance won. One shared
rule now reads both answers as a lost race, everywhere.

### Added — Garbage collection of listing-facts entries whose object is gone

The removal of listing-facts entries after a delete is best effort: its
queue is in memory, so a crash loses it, and an entry that the server stored
in the same second as the delete is kept on purpose. Such entries never give
a wrong answer, but they stayed in the bucket forever. Every six hours, each
instance now reads a part of each bucket's facts namespace (20 pages, and it
goes on from there the next time), checks the objects that the entries
describe, and deletes the entries whose object is gone or was overwritten.
An entry younger than one hour is never deleted. Set
`DGP_LISTING_FACTS_GC=false` to turn this off.

### Fixed — A filtered LIST reads at most one page budget per request

For a user whose policy covers several prefixes, the proxy lists each prefix
that the policy can see and skips the keys that the user cannot see. The limit
of 10,000 backend pages applied to each prefix, not to the request. So a
policy with many prefixes that hold many hidden keys could make one LIST read
many times that limit. The limit now applies to the whole request, including
the checks that decide which folders to show.

### Fixed — A webhook endpoint keeps its delivery state when its URL token is rotated

The proxy records the delivery result of each webhook endpoint, and a retry
posts only to the endpoints that failed. It knew an endpoint by the hash of
its full URL. When an operator rotated a token in the path or query of an
endpoint URL while an event waited for a retry, the endpoint looked new, and
the retry posted the event to it a second time. Now the proxy knows an
endpoint by its position in the list and by its scheme, host and port. The
same URL listed twice is now one endpoint. Delivery rows written by earlier
releases still count.

### Fixed — The listing-facts cleanup no longer removes a peer's new entry, and misses no deleted object

On an S3 backend, deleting a delta or encrypted object queues the removal of
its listing-facts entries. When another instance wrote the same key again
before the queue ran, the removal deleted the new entry too, so a LIST
reported the stored size of the new object until a HEAD restored the entry.
Now the removal deletes the entries that the S3 server stored before the
delete (by the `Date` of the delete response). An entry stored in the same
second or later is deleted only when it does not describe the object that is
in the bucket now. The delete of a plain
object also removes its entries now: an object from a multipart upload that
the proxy assembled has one, and it stayed forever. When `DeleteBucket`
removes the facts of an otherwise empty bucket, it now uses one batch
request per 1000 entries instead of one request per entry.

### Fixed — The replay cache costs nothing when it is off, and less under load

With `DGP_REPLAY_WINDOW_SECS=0` the replay check is off, but the proxy still
stored the signature of every mutating request, up to 500,000 entries. When
the cache is full, the proxy removes the oldest entries before the next
request. It removed them only down to the cap, so under a steady load every
following request repeated that removal. Now the check stores nothing when it
is off, the removal goes down to 90 % of the cap, and the periodic cleanup
keeps entries only for the replay window.

### Fixed — Fewer false `503 SlowDown` answers from the fenced reference write

With config sync on, the write of a delta prefix's `reference.bin` is
conditional on the version that the writer saw. Two cases gave the wrong
answer. First, when the response of that write was lost (a timeout), the
proxy sent the write again. The second write found the first one and was
refused, so the proxy answered `503 SlowDown` and removed the baseline that
it had just written. Now the proxy reads the object after a refused write:
when it holds exactly the bytes of this request, the write succeeds. The
same check covers the rewrite of only the metadata of `reference.bin`: when
the object carries exactly the metadata of this request, the rewrite succeeds.
Second, AWS answers two conditional writes that race with
`409 ConditionalRequestConflict`. The proxy read that answer as an ordinary
error that clients do not retry. Now it is a lost race, and the client gets
`503 SlowDown` and retries.

### Security — A `Deny` exception now hides keys from a LIST

A user with an `Allow` on the whole bucket and a `Deny` on part of it (for
example `Allow` `read`, `list` on `releases/*` and `Deny` on
`releases/internal/*`) could list the denied keys. The proxy classed the
listing as unrestricted, because it looked only at the `Allow` rules, so it
did not filter the keys. A listing is now unrestricted only when no `Deny`
rule can match a key under the requested prefix, and the per-key filter hides
every key that a `Deny` on `list` matches. The same classification also
ignored the actions of the `Allow` rules: a `write`-only grant on a large
prefix made the proxy scan that prefix for visible keys, and the listing
could fail before it reached the readable prefix. Only rules that grant
`read` or `list` count now.

### Changed — One multipart upload can hold at most half of the spool budget (`DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`)

A multipart upload larger than 64 MiB keeps its parts in the spool directory
until it completes. A part used to reserve only the budget that its upload did
not hold yet. So when one upload held the whole budget, each further part
reserved nothing and the proxy wrote it anyway. The disk use of that upload had
no limit, and every other request that needed spool space failed until the
upload completed or expired. Now one upload can hold at most
`DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` of spool space. The default is half of
`DGP_SPOOL_MAX_BYTES` (8 GiB with the default of 16 GiB), and `0` removes the
per-upload limit. A part that would take the upload past the limit fails with
`413 EntityTooLarge`, and the message names the variable and the limit. The
limit applies to every client multipart upload larger than 64 MiB. A part that
finds no free budget fails with `503 SlowDown`, as before.

### Fixed — Listings skip the internal listing-facts namespace

On an S3 backend the proxy keeps one small object per delta or encrypted
object under `.dg/facts/` at the root of the bucket. A dot sorts before
digits and letters, so a listing without a delimiter from the root (for
example `aws s3 sync`, rclone, or a replication walk) read every facts object
before the first user key: one backend request per 1000 facts. A listing of
the prefix `.dg/` with a delimiter also showed `.dg/facts/` as a folder. Now a
listing jumps past the facts namespace when a backend page ends inside it,
and a listing never shows a folder inside it.

### Fixed — A large request that waits for spool space no longer blocks small requests

The spool budget was a semaphore that gives free space to the request at the
head of its wait queue. While one large delta GET waited for space, every
request that must not wait (a small delta PUT, a relayed multipart part, an
encrypted upload) found no free space and failed with `503 SlowDown`, even
when most of the budget was free. Now a request that must not wait gets any
space that is free. A waiting request takes only the space that other
requests release while it waits, so it still gets its space when enough
requests finish.

### Fixed — A delta written against a replaced baseline is no longer acknowledged

With config sync on, the fence of the cross-instance reference lock covered
only the writes of `reference.bin`. A PUT that encodes a delta against an
existing baseline does not write `reference.bin`, so the fence did not check
it. When the lock of that PUT lapsed and another instance replaced the
baseline in the meantime, the delta still landed and the PUT answered 200.
The object could not be read afterwards, because its delta did not match the
new baseline. After it writes a delta, the proxy now reads the fence of
`reference.bin` again. If the baseline changed, the proxy deletes the delta
and fails the request with `503 SlowDown`, which S3 clients retry. Every
delta write goes through this check, including replication and the legacy
reference migration.

### Fixed — an SDK retry of a PUT or DELETE inside its signing second is served

When a PUT succeeded but a load balancer lost its response, the SDK retried
within the same second. SigV4 timestamps have 1-second granularity, so the
retry carried the same signature, and the replay cache rejected it with
`400 Request replay detected`: the object was stored, but the client saw a
failed upload. A duplicate PUT or DELETE that arrives less than one second
after the first copy, on the proxy's clock, is now served. A later duplicate
is still rejected, and POST requests stay strict.

### Fixed — an SSRF-refused backend endpoint is a 400

Adding an S3 backend whose endpoint the SSRF guard refuses (an `http://` or
loopback URL without `allow_local`) returned `500 Failed to rebuild engine`.
It now returns 400 with the same message, and nothing changes.

### Fixed — multipart relay parts and delta codec files count against the spool budget

Two more kinds of scratch file lived in the system temp dir, outside
`DGP_SPOOL_MAX_BYTES`. The first kind is the relay parts of large multipart
uploads, which stay on disk until the upload completes or is aborted. The
second kind is the reference copy that the delta codec writes for each
small-object encode and decode. Both now live in the spool directory
(`DGP_SPOOL_DIR`) and count against the budget. The relay directory moves from
`<system temp>/deltaglider-mpu-relay` to `<spool dir>/deltaglider-mpu-relay`.
Uploads that are in progress during an upgrade are lost, as on any restart,
and startup still sweeps the old directory. The rule is the same as for
encrypted uploads. An `UploadPart`, a multipart completion and a small delta
PUT already hold spool space or a lock, so they do not wait for more: when the
budget is in use they fail with `503 SlowDown`, and S3 clients retry. A small
delta GET holds nothing, so it waits for space up to
`DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`. Keep `DGP_SPOOL_MAX_BYTES` above the relay
parts of the multipart uploads that you expect to be open at the same time.

### Fixed — encrypted uploads keep their temporary files inside the spool budget

With proxy-AES encryption, a large upload or a multipart upload encrypts its
body into a temporary file before it goes to the backend. That file was in the
system temp directory and did not count against `DGP_SPOOL_MAX_BYTES`, so many
large encrypted uploads at the same time could fill the disk. For a multipart
upload the proxy also joined the parts into a second temporary file outside the
budget. Both files now live in the spool directory (`DGP_SPOOL_DIR`) and count
against the budget. The proxy reserves the space before it locks the
deltaspace, and an upload that already holds spool space never waits for more.
When the budget is in use, the upload fails with `503 SlowDown`, which S3
clients retry. Size `DGP_SPOOL_MAX_BYTES` for about twice the total size of the encrypted
uploads that run at the same time.

### Added — Tokio schedule-latency histogram (opt-in build flag)

A binary built with `RUSTFLAGS="--cfg tokio_unstable"` now exports
`deltaglider_tokio_schedule_latency_range_total{range}`: the number of task
wake-ups per wake-to-poll latency range. It shows ready tasks that wait for a
worker, which the poll-time series cannot show. Tokio is now at 1.53, which
added this histogram. A default build does not change. The metrics reference
explains how to read the series and what a bad value looks like (#87).

### Fixed — Lock and lease expiry no longer depends on the node clocks

The cross-instance reference lock and the replication leader lease stored an
expiry time on the writer's clock, and other instances compared it with their
own clocks. An instance whose clock ran ahead could take a lock that was
still held, and one whose clock ran behind waited on a dead lock. Expiry is
now judged by the S3 server's clock: the lock object's `Last-Modified`
against the `Date` of the response that reads it. The lock body keeps its
old fields and gains `ttl_secs`, so an instance on the previous release still
reads it; a body written by such an instance is judged as before.

### Fixed — A writer that lost the reference lock can no longer overwrite a peer's baseline

With config sync on, a PUT holds a cross-instance lock while it writes a delta
prefix's `reference.bin`. A writer whose lock lapsed (a long pause, or a clock
far ahead) could still write after a peer took the lock, and overwrite the
peer's new baseline, which orphans the peer's deltas. The write is now
conditional on the reference that the writer saw when it took the lock
(`If-Match`, or `If-None-Match: *` for a new baseline). When another writer
changed it in between, the PUT fails with `503 SlowDown`, which SDKs retry,
instead of overwriting it. Backends without conditional writes (Backblaze B2)
keep the old unconditional write; they are refused for multi-instance delta
storage anyway.

### Changed — A replayed mutating request is refused for its whole valid life

The replay window was 2 seconds, so a captured PUT or DELETE replayed after
2 seconds, but inside the 900-second clock-skew tolerance, was accepted. The
window now defaults to the clock-skew tolerance (`DGP_CLOCK_SKEW_SECONDS`).
GET and HEAD signatures no longer enter the replay cache, because a replayed
read has no effect and SDKs repeat read signatures within one second. A
request that fails still gives its slot back, so SDK retries work.
`DGP_REPLAY_WINDOW_SECS` still sets another window, and `0` switches the
check off.

### Changed — lifecycle run-now starts the run in the background

`POST /_/api/admin/jobs/lifecycle:<name>/run-now` waited until the rule
finished, so a rule over a large bucket held the admin request open for the
whole sweep. The endpoint now works like replication run-now: it takes the
rule lease, opens the run-history row, starts the run in the background, and
answers `202` with `run_id` and `status: "running"`. Poll
`GET /_/api/admin/jobs/lifecycle:<name>/runs` for the result. The Jobs screen
does this for you: the Runs tab refreshes while a run is in progress. A client
that read the counters from the run-now response must read them from the run
history instead.

### Added — per-endpoint delivery state in the event log

Each row of `GET /_/api/admin/event-outbox` now carries `deliveries`: for
each webhook URL or Slack channel, whether it accepted the event, the number
of attempts, and the last error. URLs are redacted. On the Event log page,
the status cell shows how many endpoints accepted the event, and you can
expand a row to see each endpoint.

### Fixed — a Slack retry posts only to the channels and URLs that failed

With several Slack Incoming Webhook URLs, a retry posted the message again to
the URLs that had already accepted it. In bot-token mode, a message that
reached at least one channel counted as delivered, so a channel that failed
never got it. Slack delivery now uses the same per-target state as raw
webhooks (`event_deliveries`, one row per URL or channel): each failed URL or
channel retries on its own, and no channel gets the message twice.

### Fixed — a raw webhook retry posts only to the endpoints that failed

With several `webhook_urls`, the dispatcher posted to the endpoints in order
and stopped at the first failure. The endpoints after it did not get the event
until the failing one recovered, and every retry posted the event again to the
endpoints that had already accepted it. Now the proxy records the result of
each endpoint in a new `event_deliveries` table (schema v27). A failing
endpoint does not stop the others, and a retry posts only to the endpoints
that have not yet succeeded. Upgrade: rows that are pending at upgrade time
have no per-endpoint history, so their next attempt posts to every endpoint
once more.

### Changed — IAM config sync merges changes from every instance

Before, a config-sync download replaced the whole IAM table set with the copy
in the bucket. When two instances changed IAM at about the same time, one
instance lost its change, for example a user that it had just created. Now
each instance keeps the database that it last shared with the bucket
(`deltaglider_config.db.sync-base`) and uses it as the base of a three-way
merge. Rows are matched by name. A change that one side made is kept, and a
delete on one side is a delete on both sides. When both sides change the same
row, the more recent change wins, a delete wins over an edit, and the proxy
writes an `iam_sync_conflict` audit entry. External identities, group members,
and mapping rules follow their user or group by name when an id changes.
Mapping rules have no name, so schema v28 gives each rule a `rule_uid`. When
two instances edit the same rule, the merge keeps one rule (the more recent
edit) instead of both versions. The upgrade derives the uid of an existing
rule from its content, so equal rules on two instances get the same uid.

When both sides change the same row, the merge compares it column by column
with the base (the permissions count as one column), so a key rotation on one
instance and a permission edit on the other both survive; only a column that
both sides changed goes to the more recent change. A rename of a user, group,
or provider is matched by the row's id and creation time, and it is applied
to the other side before the merge, so a login or a membership change on the
other instance still reaches the renamed row. Two users that two instances
created with one name and different access keys (for example two people with
one IdP display name) stay two users: the one whose access key sorts later is
renamed to `<name>-<tag>`, where the tag is the first 8 hex characters of
the SHA-256 hash of its access key id, on every instance alike. OAuth provisioning of a taken name uses the same suffix
instead of `-2`. When the merge would give one access key to two users (for
example two rotations to one imported key), it no longer deletes one of them:
the more recent user keeps the key, and the other one is disabled with a
stand-in key (`DISABLED-DUPLICATE-...`) and keeps its name, secret,
permissions and memberships, so that an admin can give it a new key. An
`iam_sync_conflict` audit entry names both users. A failed commit of the
merge rolls back.

Upgrade: the schema moves to v26 and v28, which add a `sync_mtime` column to
the IAM tables and a `rule_uid` column to the mapping rules. The first sync
after the upgrade has no merge base, so the merge keeps every row of both
sides: a create or edit that was not synced yet survives, and a delete that
was not synced yet comes back. An instance reads the schema version of a
synced database before it migrates it: it refuses a newer one, and it merges
an older one after it migrates a copy. The rows of an older copy have no
change time, so a conflict with them goes to the copy in the bucket.

### Changed — The IAM database has its own encryption key (upgrade step for multi-instance)

The encrypted config database (`deltaglider_config.db`) used the bootstrap
password hash as its SQLCipher key. So anyone who could read the config file
could decrypt the IAM database, and `--set-bootstrap-password` made the
database unreadable. The database now has its own key: `DGP_CONFIG_DB_KEY`,
or, when that variable is unset, a random key in the file
`deltaglider_config.db.key` next to the database (mode 0600), which the proxy
generates on the first start. The bootstrap password only signs admin
sessions and gates the admin GUI. `--set-bootstrap-password` and
`PUT /_/api/admin/password` no longer touch the database.

The first start after the upgrade opens the database with the bootstrap
password hash and re-encrypts it with the new key. The re-encryption works on
a copy, and the copy replaces the original only after it opens with the new
key, so a failure leaves the original unchanged. Back up the key together
with the database from now on: a database without its key cannot be read.

Upgrade steps:

- **One instance:** nothing to do. Back up `deltaglider_config.db.key` with
  the database.
- **Several instances with `config_sync_bucket`:** generate one key
  (`openssl rand -hex 32`) and set it as `DGP_CONFIG_DB_KEY` on every
  instance before you start this release. An instance with a sync bucket and
  without the variable refuses to start, and the admin API refuses to set or
  change `config_sync_bucket` on an instance without the variable. Upgrade all instances together and
  make no IAM changes during the rollout: instances on the old release cannot
  read uploads under the new key. For the rollout, also set
  `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true` on every instance, so that the new
  release accepts a synced database that an old instance wrote under the hash,
  and remove it when the rollout is complete. Without it, a synced database
  that opens only with the hash is refused: the hash is in configuration files
  and backups, so anyone who can write to the bucket and knows the hash could
  plant an IAM database. An instance whose key
  differs refuses the synced database with an error that names
  `DGP_CONFIG_DB_KEY`, and never overwrites the synced copy.
- **Kubernetes operator:** with `bootstrapPassword.autoGenerate`, the operator
  adds a `dbKey` to the `<name>-bootstrap` Secret and injects it as
  `DGP_CONFIG_DB_KEY`. Without it, add `DGP_CONFIG_DB_KEY` to the env Secret;
  the operator does not scale beyond one pod without it.
- **Helm:** set `auth.configDbKey` (or `DGP_CONFIG_DB_KEY` in
  `auth.existingSecret`) before you raise `replicaCount` with config sync.

To rotate the key later, set `DGP_CONFIG_DB_KEY_PREVIOUS` to the old key and
`DGP_CONFIG_DB_KEY` to the new one, and restart. A database, a sync merge
base, or a synced copy that opens only with the previous key is re-encrypted
with the new key. The synced copy follows: an instance uploads its database
once when the database changed key at start, after a recovery promotion or
`--set-bootstrap-password` re-encrypted it, and when it merged a synced copy
that opened only with the previous key.

A full backup restore no longer refuses a backup whose bootstrap password
hash differs from the running instance's: the hash no longer protects the
database, so the restore adopts the backup's admin password. When
`DGP_BOOTSTRAP_PASSWORD_HASH` is set, the running password stays, because
that variable sets it at every start.

The recovery wizard of a locked database now accepts a config DB key, or,
for a database from an older release, the bootstrap password hash.

### Changed — `DELETE photos/` deletes only the folder marker, as on S3

A `DeleteObject` request for a key that ends in `/` deleted every key under
that prefix. On S3, the same request deletes only the object named `photos/`,
which S3 clients create as the marker of an empty folder. A client that
cleaned up a folder marker lost the whole folder. The proxy now deletes only
the marker, and a missing marker is a success with nothing deleted. To delete
a folder, list its keys and delete them with `DeleteObjects`: the object
browser, `aws s3 rm --recursive`, and `deltaglider_proxy s3 rm -r` all do
this. The environment variable `DGP_RECURSIVE_DELETE_PAGE_SIZE` is gone.

A `PutObject` request for a key that ends in `/` failed with
`InvalidArgument`. It now stores a zero-byte folder marker, and a listing shows
the marker as a zero-byte object. A request with a body for such a key still
gets `400 InvalidArgument`. On the filesystem backend, the marker is a file
named `.dg-folder-marker` inside the folder, so a key whose last part is
`.dg-folder-marker` is refused there. `deltaglider_proxy s3 rm -r` now also
deletes the folder markers under the prefix and the marker of the folder
itself, unless `--include` or `--exclude` narrows the delete.

### Fixed — A prefix-scoped user could not list keys that sort after many hidden keys

A user whose policy allows only some prefixes of a bucket, such as
`releases/team-a/*` and `releases/team-z/*`, got a listing that the proxy
filtered key by key. To fill a page, the proxy read the whole bucket in key
order and skipped the hidden keys, up to a limit of 10,000 backend pages. When
more hidden keys than that sorted between two visible ones, every request for
the next page failed with `InvalidRequest`, so the later visible keys could not
be listed. The proxy now reads only the prefixes that the user's `Allow` rules
can reach and merges those listings in key order. Hidden keys outside those
prefixes cost nothing, and every visible key can be listed. A policy that
cannot be narrowed to prefixes, such as an `Allow` on the whole bucket with
`Deny` exceptions, still uses the limited scan.

### Changed — CLI: recursive prefixes are directories, and `rm` globs are relative

`deltaglider_proxy s3 rm -r s3://releases/v2` deleted the keys under `v2/`
and also the keys under every sibling that starts with the same letters, such
as `v2-rc/`. The CLI listed the raw prefix `v2`, and S3 returns every key that
starts with it. `cp -r`, `sync`, and `migrate` had the same fault; `migrate`
also wrote destination keys with `//` in them. These verbs now treat a
non-empty prefix as a directory: `v2` and `v2/` both select only `v2/...`.

`rm -r --include` and `--exclude` globs now match the key relative to the
prefix, the same as in `cp -r`, `sync`, and `migrate`. A glob that names the
full key, such as `releases/tmp/*`, must drop the prefix and become `tmp/*`.
A glob without a `/`, such as `*.zip`, works as before.

### Fixed — CLI downloads could write outside the destination directory

A key such as `pre//etc/cron.d/evil` made `cp -r` and `sync` write to
`/etc/cron.d/evil`, because the key below the prefix was an absolute path.
Keys with `..` segments escaped the same way. The CLI now skips, with a
warning, every key that could resolve outside the destination directory.

### Fixed — CLI: session tokens, copied metadata, and `verify` on foreign objects

- The `s3` verbs now send `AWS_SESSION_TOKEN` (or the profile's
  `aws_session_token`) with every request, so temporary (STS) credentials
  work. Before, the CLI read the token and did not send it.
- `cp`, `sync`, and `migrate` between two S3 locations now keep the source
  object's user metadata. A `--metadata` flag on `cp` replaces one key.
- `s3 verify` on an object that another tool wrote reported `MISMATCH`
  (exit `9`), because the object has no DeltaGlider checksum. It now reports
  `UNVERIFIABLE` and exits `0`.

### Fixed — Listings reported the stored delta size instead of the object size

A `ListObjects` request on an S3-backed bucket reported each delta object with
the size and ETag of its stored delta, not of the object itself. A LIST
returns only what the backend stores, and the original size and ETag live in
the object's metadata, which a LIST does not return. A 3 MB `firmware.tar` was
listed as `46 B`, the object browser showed that size, and a sync tool that
compares sizes copied the object again on every run. Objects on a
proxy-encrypted S3 backend had the same fault: they listed with the size and
ETag of the encrypted data.

Every upload that stores an object in another form (a delta, or encrypted
data) now also writes an empty index object under `.dg/facts/` in the same
bucket. Its key holds the stored object's key, ETag, and size, and the
original size and ETag. A listing page reads these index objects with one more
listing request and reports the original size and ETag, on every proxy
instance and after a restart. An index object applies only to the stored
object with exactly that key, ETag, and size, so a stale one is ignored. An
overwrite removes the index objects that the backend stored before the new
one, so when two proxy instances overwrite the same key at the same time, the
index object of the later write stays. A delete removes the object's index
objects in the background: the proxy collects the deletes for a moment, reads
the index range of each folder once, and removes the entries with batched
`DeleteObjects` requests. A delete of 1000 objects then costs a few requests,
not 1000. Listings never show `.dg/facts/`, and an upload to a key under
`.dg/facts/` gets `400 InvalidArgument`. A tool that lists the backend bucket
directly (not through the proxy) sees these zero-byte objects; a delete of an
empty bucket through the proxy removes them first.

An object stored before this release lists with its stored size until the
proxy reads its metadata once, with a download or a `HEAD` request, on a
proxy that listed it before. The proxy then writes the index object in the
background. A `metadata=true` listing now sends a `HEAD` request for each
encrypted object whose index object is missing, so it reports the original
size too.

The proxy also remembers the original size and ETag of each stored object that
it writes, reads, or lists, so a page whose objects are all in this cache sends
no extra request. The cache holds about 120,000 objects by default (for
60-byte keys); `DGP_LIST_SIZE_CACHE_MB` (default `32`) sets its size.
`LastModified` in a listing is always the time that the backend reports, so it
does not change between two listings of the same object. The new counter
`deltaglider_backend_head_requests_total` counts every object metadata (HEAD)
request that the proxy sends to S3: to the storage backends and to the
config-sync bucket. The new counter `deltaglider_listing_facts_requests_total`
counts the requests for the index objects.

### Fixed — Folder sizes in the object browser showed the stored size

The folder-size scan added up the stored sizes, so a folder of deltas showed a
few hundred bytes. The scan result (`GET /_/api/admin/usage`) now reports:

- `total_size` and each child's `size`: the original size of the objects, the
  same as the size of a file.
- `stored_size`, on the total and on each child: the bytes stored on the
  backend, including the delta baselines. The quota fallback uses this value.
- `sizes_estimated`, on the total and on each child: `true` when the original
  size of some objects is not known to this proxy (see the entry above). Those
  objects count their stored size instead, which is smaller for a delta and
  slightly larger for an encrypted object, so the total is approximate. The
  object browser then shows the size as `≈ 12 MB`. For a scan that stopped at
  its object limit (`truncated`), it shows `≥ 12 MB`.
- `age_seconds`: the age of the result. `stale_seconds` is now `0` while the
  result is fresh instead of a negative number.

The scan makes one listing of the folder and no other requests. It reads the
baseline sizes from that listing, so it no longer lists the whole bucket and
reads each baseline separately.

### Fixed — Filesystem listings missed keys when the prefix ended inside a name

A listing without a delimiter on a filesystem backend returned no keys when the
prefix ended inside a name (for example `nightly/pg`), while the S3 backend
returned the matching keys. Both backends now return the same keys.

### Fixed — A ZIP download hid the files it could not read

The bulk ZIP download of the object browser skipped each file it could not
read without saying so, and when it could read none of them it returned an
empty archive. A partial archive now contains `_deltaglider-skipped-files.txt`,
which lists each missing file and the reason. If a selected file already has
that name, the list gets a free name such as `_deltaglider-skipped-files-2.txt`.
When no selected file can be read, the request fails: with `404` when every
file is missing, with `403` when the backend denied access to any file, with
`413` when a file is too large, with `503` when the proxy or the backend is
overloaded, and with `502` for other backend failures.

### Changed — A new filesystem backend needs an absolute path

`POST /_/api/admin/backends` rejects a filesystem backend without a path or
with a relative path. A relative path resolves against the working directory
of the proxy process, which is different under systemd, Docker and a shell, so
the data could land in an unexpected place. The admin UI already required an
absolute path.

### Fixed — Copy events reported the stored delta size

`ReplicationObjectCopied` and `LifecycleTransitioned` events reported the
number of bytes that the copy transferred in `payload.content_length`. When
the copy sends the stored delta, that is the delta size, for example `43` for a
1 MB object. The field now holds the size of the object.

### Fixed — Successful admin logins were not audited

Only failed logins wrote an audit entry. Every successful login now writes one:
`login` for the bootstrap password, `login_as` for an IAM administrator, and
the existing `external_login`, `browser_session_connect` and
`open_browser_connect` entries for the other login paths.

### Fixed — The event outbox grew forever while delivery and replication were off

Every object write adds a row to the event outbox. While event delivery was off
and replication was disabled, nothing ever deleted those rows, so the encrypted
config database grew by one row per write. The dispatcher now deletes every row
that no reader needs. While replication is enabled, a row is deleted only after
replication has read it. A row that delivery already tried (a pending retry or
a failed delivery) is kept while delivery is off, so that an operator who turns
delivery off to repair an endpoint can still requeue those events afterwards.

### Changed — The apply dialog separates new warnings from existing ones

The "Review & apply" dialog showed every warning of the resulting
configuration, so a standing warning (for example the rate-limit warning about
`DGP_TRUST_PROXY_HEADERS`) appeared on every unrelated change. The section
validate and apply responses now return the warnings that the change introduces
in `warnings` and the warnings that the current configuration already produces
in `existing_warnings`. The dialog shows the new warnings prominently and folds
the existing ones away.

### Security — Authentication and authorization fixes

This release closes several authentication and authorization defects. Upgrade
promptly.

- A request is now honoured only when the S3 signature layer verified the same
  access key that the proxy resolved the caller to. Before this fix, a request
  that carried two `Authorization` headers, or a SigV2 query string next to a
  SigV4 header, could act as any user whose access key ID the caller knew.
- Authorization, admission blocks and the maintenance write gate now check the
  bucket, key and query exactly as the S3 layer decodes them. Before this fix,
  one percent-encoded character (for example `%2F`, `%74`, or `%70refix=`), or
  an extra `/` in front of the key, made the policy check a different resource
  from the one that the proxy served.
- An OAuth or OIDC login now receives an admin session only when the user is an
  admin, through direct or group permissions. Other users receive the
  browser-only session, which is the same session that an IAM non-admin
  receives from browser-connect.
- Every admin request now checks that the session's user is still an enabled
  admin. A user who is disabled or demoted loses the admin API on the next
  request.
- User names that start with `$` are reserved for built-in principals. The
  admin API, backup import and OAuth provisioning now refuse or strip them.
- On the filesystem backend, a key or a list prefix with a `.`, `..` or empty
  segment (for example `a/./secret.txt`, `a//secret.txt` or `prefix=a/./`) is
  now refused with HTTP 400. The filesystem resolves such a key to the file of
  `a/secret.txt`, while the policy check saw the literal key, so a Deny on
  `a/secret*` did not apply to reads, writes or listings. The check is in the
  filesystem backend itself, so replication and lifecycle writes into a
  filesystem bucket follow it too. The S3 backend stores keys literally and is
  not affected. On S3, a new key with an empty segment just before the file
  name (`a//x`) is now refused on upload, as other `//` keys already were.
- The admin log stream and the bucket scan stream now end within about one
  second when their session stops being an admin session. Before this fix, a
  disabled or demoted admin, or a revoked session, kept receiving every server
  log line until it disconnected.
- User names are now unique. Before this fix, an OAuth or OIDC user could pick
  an identity-provider name equal to another user's name and so share that
  user's `${iam:username}` prefix. OAuth provisioning now adds a suffix to a
  name that is in use (`dana-2`), and the admin API answers HTTP 409 to a
  create, clone or rename that would duplicate a name (a group rename too). The
  database upgrade renames all but one user of each existing same-name group
  and logs a warning: a local user keeps the name before an OAuth user, and
  otherwise the older user keeps it. A backup import renames a colliding user
  in the same way instead of skipping it. In declarative mode, a YAML user
  that has the name of an OAuth-created user but its own access key fails the
  apply. A user name
  of `.` or `..` no longer expands in `${iam:username}`.
  **Downgrade note:** the upgrade adds the unique index `idx_users_name`, and an
  older version keeps it. Drop the index before you downgrade, or an older
  version fails OAuth logins whose name is in use.

### Fixed — Replication run-now and the event consumer use the shared lease

With a coordination bucket configured, the replication scheduler took its
per-rule lease in the bucket, but the event consumer and the admin run-now took
a lease in the node-local database. So they did not exclude each other: a
reconcile run could delete, as an orphan, a copy that the consumer made during
the run. All three now take the same lease, and deleting a rule takes it too
for the duration of the purge. A single instance keeps the node-local lease, as
before.

- With the S3 lease, a worker could take a live lease from another worker of
  the same process, because a node could always reclaim its own lease. Now a
  node reclaims only a lease that an earlier process wrote (after a restart).
- The event consumer re-checks at every key whether the rule is paused or
  deleted. A key that the destination can never accept (for example a `.`
  segment on a filesystem destination) is recorded as a failure and no longer
  holds the event cursor for every rule.
- While another node runs a reconcile of a rule, the event consumer holds that
  rule's events, and the cursor with them, until the run ends. This keeps the
  consumer from copying objects that the run is about to delete.

### Changed — `${iam:username}` matches names that contain punctuation

Because authorization now compares policies against the decoded key, the
`${iam:username}` and `${iam:access_key_id}` templates insert the name as it
is. A policy on `home/${iam:username}/*` now matches the key
`home/dana@corp.com/report.pdf` for the user `dana@corp.com`. A name or access
key that contains a character that changes the meaning of a pattern (`/`, `*`,
`?`, `$`, `{`, `}` or `%`) cannot be inserted safely. If one of a user's
effective permissions uses the template, that user now gets no permissions at
all, and the proxy logs a warning. Rename such users to keep their access.

### Changed — A paused replication rule also pauses event-driven replication

Before this release, pausing a rule stopped only the scheduled reconcile run.
The event consumer still copied new objects and propagated deletes. Now a
paused rule does nothing, and the events for it during the pause are not
queued. Resuming the rule makes it due at once, so the next scheduler tick
starts a full reconcile run (from the start, not from a saved position) that
brings the destination in sync. With
`replicate_deletes: true`, that run applies the source's deletes too; to keep
deleted objects on the destination, turn `replicate_deletes` off.

### Changed — The persisted config file is no longer world-readable

The config file holds credentials and encryption keys. When the proxy writes
it, a new file now gets mode `0600`, and a rewrite keeps the owner and group
permissions of the old file but removes all permissions for other users. An
existing `0644` file therefore becomes `0640` on its next write.

### Changed — The build version is no longer advertised to anonymous callers

`GET /_/api/whoami` returns `version` only when the request carries a live
session. The login page still receives the auth mode and the list of external
login providers, which it needs before any login, but an unauthenticated caller
can no longer read the exact running version. The `version` label of
`deltaglider_build_info` on the unauthenticated `/_/metrics` endpoint is now
empty by default, which Prometheus treats as an absent label, so existing
dashboards keep their series shape. Operators whose fleet dashboards key on the
version can restore the label with `DGP_METRICS_EXPOSE_VERSION=true`. The admin
dashboard reads the version from the authenticated identity instead of the
scrape.

### Changed — Nothing served to anonymous callers identifies the build

A deployment could still be fingerprinted from surfaces that need no login.
Every one of them is now closed:

- The JS bundle embedded a build version and a build timestamp as Vite
  constants. Both are gone. The sidebar and the dashboard read the running
  version and build time from `GET /_/api/whoami`, which reports them only to a
  live session.
- The product docs, including the changelog that names every release, were
  inlined into the bundle as static assets. They are now embedded in the
  binary and served by `GET /_/api/docs` to a live session only; the docs
  viewer fetches them at runtime. The canned IAM policy catalogue
  (`/_/api/admin/policies`) moved behind the session for the same reason.
- Every object the proxy stores carries `dg-tool = deltaglider_proxy/<version>`
  as provenance, and a HEAD, GET, or `metadata=true` LIST returned it as
  `x-amz-meta-dg-tool` to anonymous readers of a public prefix. Those
  anonymous requests no longer receive that key; authenticated principals,
  and open-access mode (`authentication: none`, where nothing is private
  and the proxy's own tooling reads the `dg-*` keys), still do.
- `DGP_METRICS_BEARER_TOKEN` (new, optional) makes `/_/metrics` require
  `Authorization: Bearer <token>` — the Prometheus `authorization:` scrape
  setting — or an admin session. Unset keeps the endpoint public.
- A build guard, `scripts/check-bundle-fingerprints.sh`, runs in the Docker
  image build and in CI after the UI build. It fails the build if the
  bundle contains the crate version, a build timestamp, inlined docs, or a
  source map.

The static asset names still carry a content hash, third-party libraries
embed their own version strings, and a presigned URL for an object returns the
writer's `dg-tool` metadata to whoever holds the link (a signed request, not an
anonymous one). Those identify a build only to someone who
already holds a copy of every release; nothing in the bundle states the
version outright.

### Changed — No build of the proxy ships frontend source maps

A source map in the embedded bundle handed the full UI source to anyone who
could reach `/_/`. Source maps are now opt-in for the UI build
(`DGP_UI_SOURCEMAP=1`), so the Docker image, the release tarballs and the
demo image all embed none without any per-channel flag. A local debugging
build sets the variable.

### Fixed — Unknown paths under `/_/` returned the app shell with status 200

The catch-all route for the embedded UI served `index.html` for every path it
did not recognise, including mistyped admin API paths such as
`/_/api/admin/does-not-exist` and missing assets. API clients received a 200
HTML page instead of an error, and scanners saw every probe succeed. Only the
genuine client-side routes (`browse`, `upload`, `metrics`, `docs`, `admin`) now
fall back to the app shell. Unmatched paths under `/_/api/` return a JSON 404
for every HTTP method, and everything else returns a plain 404; both carry
`Cache-Control: no-store` so a cache in front of a mixed-version fleet never
keeps a 404 for an asset a newer instance serves.

### Changed — MinIO images for tests and the demo come from `pgsty/silo`

The `minio/minio` and `minio/mc` images disappeared from Docker Hub in
September 2026. The integration tests, the nightly run, the two Docker
Compose files, and the Docker Hub README example now start `pgsty/silo`, a
maintained fork of the MinIO server that keeps the `MINIO_*` variables, the
`server /data` command, the S3 and admin APIs, and the storage format, and
bundles the client as `mcli`. Nothing changes for the proxy itself or for
deployments that run their own MinIO.

## v1.19.0 — 2026-08-10

### Added — Cross-instance protection for the delta reference baseline

Each deltaspace keeps one `reference.bin` baseline, and every delta in that
prefix is decoded against it. Until now the read-modify-write that creates
this baseline was guarded only by a lock held inside a single process, so two
proxy instances that wrote into the same prefix at the same time could each
observe that no reference existed and each write a baseline. The second write
replaced the first, and every delta that pointed at the replaced bytes became
unreadable. Multi-instance deployments therefore depended entirely on the
operator routing every request for a prefix to the same instance.

When a coordination bucket is configured (`config_sync_bucket`), the proxy now
also takes a cross-instance lock around that read-modify-write. The lock is a
short-lived object in the coordination bucket, created with a conditional write
so that exactly one instance can hold it, and released as soon as the baseline
is written. A holder that dies without releasing the lock stops blocking other
instances once the lock expires. If another instance holds the lock for longer
than the acquire timeout, the upload fails with a clear error instead of
risking a second baseline.

Single-instance deployments are unaffected and pay no additional requests: with
no coordination bucket the in-process lock remains the only lock. Two settings
tune the behaviour: `DGP_REFERENCE_LOCK_TTL_SECS` (default 120) and
`DGP_REFERENCE_LOCK_ACQUIRE_TIMEOUT_SECS` (default 30).

### Fixed — Anonymous listings could reveal keys under private sibling prefixes

A public prefix such as `releases/` grants anonymous read and list access to the
objects below it. The per-key filter that enforces this compared the requested
prefix, rather than each returned key, against the grant. An unauthenticated
request for `?prefix=releases` without a delimiter therefore returned every key
that begins with that text, which includes a private sibling prefix such as
`releases-internal/`. Object contents stayed protected, but the key names and
their metadata were disclosed. The filter now evaluates each key, so a sibling
prefix that merely shares the name is excluded.

### Fixed — Server-side copies of large objects could exhaust proxy memory

`UploadPartCopy` read the whole source object into memory before it applied the
requested byte range, and it did not check the source size first. Because
pass-through objects may be stored up to 64 GiB, a routine multipart copy issued
by the AWS CLI or by boto3 could exhaust the memory of the proxy process. The
handler now rejects an oversized source with `EntityTooLarge`, which matches the
check that `CopyObject` already performed.

### Fixed — Webhook events could be discarded before they were delivered

The event outbox pruned rows below the position reached by the replication
consumer on every delivery pass, without regard to delivery status. When a
webhook target was unavailable and its events were waiting for a retry, those
events were removed before they were delivered, which broke the at-least-once
guarantee. The prune no longer runs while delivery is active; the existing
prunes for delivered and failed rows continue to bound the size of the outbox.

### Fixed — Replicated copies of encrypted objects could be unreadable

Replication and lifecycle copies carried the encryption markers of the source
object into the destination. When the destination does not encrypt on write,
those markers described an encryption that the stored bytes no longer had, and
reads of the replica failed. The copy paths now remove the markers, which is
what the client-facing copy handler already did.

### Fixed — A transient backend error could route a multipart upload elsewhere

While resolving which backend holds a bucket, a failed existence request was
treated as proof that the bucket was absent, and the request continued to the
next backend. A short outage on the backend that actually holds the bucket could
therefore send an upload to a different backend with the same bucket name. The
resolver now distinguishes a failed request from a negative answer and stays on
the backend that returned the error.

### Changed — Object writes and prefix deletions are substantially faster

Two problems on the write path are corrected. The counter database that records
per-bucket usage was opened with the default SQLite settings, so every object
upload and every deletion waited for its own flush to disk. It now uses
write-ahead logging, which removes that wait. Separately, deleting an object
listed the entire deltaspace to decide whether the reference baseline could be
reclaimed; during a recursive deletion this repeated for every object, so the
work grew with the square of the object count. The reclamation now runs once,
after the sweep. Deleting a prefix of 1100 objects previously exceeded the
request timeout and returned an error; it now finishes in about 45 seconds.

### Changed — The supply-chain gate is enforced again

`cargo deny` was configured for a version of the tool that no longer accepts
those settings, so the check failed to start, and the nightly workflow ignored
its result. The configuration is updated, the result is no longer ignored, and
the `rand` dependency is updated to clear an advisory. Known advisories that
have no available fix are listed explicitly, so any new advisory fails the
workflow.

### Fixed — Documentation that did not match the software

The Docker quick-start commands omitted the credentials that the proxy requires,
so the container stopped immediately on startup; the credentials are now
included and explained. The example configuration showed a TLS section without
`enabled: true`, so following it produced plain HTTP while appearing to
configure HTTPS; the required setting is now shown.

## v1.18.0 — 2026-08-08

### Added — Signed release provenance and a published security policy

Every release now carries Sigstore-backed build provenance: the binary
tarballs, the SHA-256 checksum file, and the SBOM are attested by the
release pipeline, and any downloaded artifact can be verified with
`gh attestation verify <file> --repo beshu-tech/deltaglider_proxy`.
A new `SECURITY.md` documents private vulnerability reporting (GitHub
advisories or email), concrete response targets — acknowledgement
within 2 business days, a fix or documented mitigation for critical
issues targeted within 14 days — and direct notification for
Commercial-plan customers when a security release ships. Together with
the SBOM that releases already carried, this makes the "signed builds,
SBOM, CVE response commitment" line on the pricing page a shipped fact
rather than a promise.

### Changed — License: GPL-3.0 → Business Source License 1.1

DeltaGlider Proxy is now licensed under the Business Source License 1.1
(BUSL-1.1). What this means in practice:

- **Production use stays free for most deployments.** The license grants
  free production use as long as the total compressed data stored through
  the proxy stays under 15 TB per organization, and the proxy itself is
  not resold as a hosted or managed service. Development, testing, and
  evaluation are free at any size.
- **Every release becomes open source on a schedule.** Two years after
  each version is first released, that version automatically converts to
  the Apache License 2.0.
- **Nothing is gated.** There are no license keys, no feature flags, and
  no phone-home. The full product ships to everyone; the license is a
  legal term, not a technical lock. The admin dashboard already shows
  your compressed footprint, so you can see for yourself which side of
  the 15 TB line you are on.
- **Old releases are unaffected.** Every release up to and including
  v1.17.0 was published under GPL-3.0 and remains under GPL-3.0 forever.

Deployments above the 15 TB grant, embedding in proprietary products,
and hosted-service resale require a commercial license — see
[deltaglider.com/pricing](https://deltaglider.com/pricing/). The pricing
model changed with the license: the TB-bracketed support tiers are gone,
replaced by a single flat Commercial plan that bundles the license,
signed builds, support, and everything else.

### Changed — operator 0.2.4 and Helm chart defaults track v1.17.0

The operator's pinned default proxy image and the Helm chart's
`appVersion` now point at v1.17.0, so a spec without an explicit `image`
deploys the release with the multipart-completion resilience fixes. The
Kubernetes guide's note about in-cluster DNS backends now names the
versions precisely: v1.17.0 and later accept `*.svc.cluster.local`
endpoints when `DGP_BACKEND_ALLOW_LOCAL` is set; older releases need the
Service's ClusterIP.

## v1.17.0 — 2026-08-04

### Fixed — CompleteMultipartUpload survives client disconnects and tolerates retries

Found by killing a load-balancer mid-request on a live cluster: when the
client's connection dropped while `CompleteMultipartUpload` was being
processed, the cancelled request future abandoned the half-done store,
the upload state rolled back, and the SDK's automatic retry was refused
with `InvalidRequest: Upload is already being completed` — the upload was
destroyed and the retry poisoned. This could be triggered by any dying
proxy hop or flaky network, on single-instance deployments too. The store
pipeline now runs on a detached task that a disconnect cannot cancel, and
a completion registry makes retries safe: an identical retried Complete
joins the in-flight completion (or returns the finished result from a
success tombstone), a retry with a different part list is refused, and a
failed completion clears the way for a fresh attempt. Covered by
deterministic disconnect/race tests (`DGP_TEST_COMPLETE_STALL_MS` hook).

### Fixed — one proxy's boot can no longer destroy another proxy's in-flight multipart parts

Large multipart parts are relayed to a directory under the system
temporary folder, and that directory was shared by every proxy process on
the host. The startup sweep that cleans crash leftovers deleted
everything it did not recognise — including the LIVE relay parts of
another proxy running on the same machine, whose upload then failed with
"Failed to persist relay part". Relay roots are now per-process; leftover
roots of dead processes are still reaped, but only after they have been
stale for an hour (tunable via `DGP_RELAY_FOREIGN_MIN_AGE_SECS`). Two
identical racing Completes also now both return the same success instead
of one being rejected (a consequence of the completion registry above),
and the concurrency test asserts the new converging contract.

### Changed — HA documentation states the full failure model

The scale-out guide now lists all four events that move the hash ring
(scale up, scale down, pod restart, readiness ejection) with their blast
radius, the all-or-nothing nature of per-pod readiness, and the admin
GUI's effective single-pod behaviour. The multi-instance guide gains the
per-instance state that was previously only in internal notes: metadata
cache staleness (and why directory-hash routing mostly neutralises it)
and rate-limit multiplication. Operator 0.2.3's status message now points
at backend connectivity when routers are up but no proxy pod is ready.

### Fixed — dev-mode backend guard now accepts in-cluster Kubernetes DNS names

The SSRF guard rejected every hostname ending in `.local` or `.internal`,
even with `allow_local: true` / `DGP_BACKEND_ALLOW_LOCAL=true` — and
Kubernetes in-cluster service names all end in `.svc.cluster.local`, so an
in-cluster MinIO or Ceph backend was unreachable by name (the error even
suggested the flag that then changed nothing). Dev mode now permits the
forbidden name suffixes while still refusing the named cloud-metadata
hosts; production mode is unchanged and still rejects them all. Found by
running the operator against a real cluster.

### Fixed — Kubernetes operator: 0.2.2

Three defects found by actually deploying the operator (none of them
reachable by unit tests): the published images were built on a newer
Debian than their runtime and died at exec with a glibc version error
(builder now pinned to bookworm to match the distroless runtime); the
status readers used the `/status` subresource, which needs an RBAC grant
the ClusterRole never had, so every deployment reported `0/N Progressing`
forever (now reads the whole object, which the existing grant covers);
and every documented `configYaml` sample kept the backend credentials
only in the env Secret, which a YAML-defined backend never reads — the
samples now use `${env:...}` references, which the proxy expands inside
the pod so credentials reach the backend without ever sitting in the
ConfigMap.

### Added — official Kubernetes operator with multipart-safe multi-pod routing

New `operator/` crate: a Kubernetes operator (CRD `DeltaGliderProxy`,
group `deltaglider.beshu.tech/v1alpha1`) that manages the proxy StatefulSet
(one PVC per pod), the config, the Services — and an HAProxy router that
**consistent-hashes S3 traffic by the directory of the URL path** (the
deltaspace). That routing is what makes a multi-pod deployment work with S3
multipart uploads: multipart state is per-pod, so behind a round-robin
Service the SDK's parallel `UploadPart` requests hit the wrong pods and
fail with `NoSuchUpload`. Directory-pinning routes every request that
touches one delta prefix — all keys in it, every multipart part — to the
same pod, which both keeps multipart uploads working and satisfies the
delta engine's single-writer-per-deltaspace reference lock (per-key
hashing would not: two keys in one prefix share one reference file).
Routing behaviour verified against haproxy:3.0.

Operator 0.2.0 lowers the friction of getting to a correct multi-pod
deployment. The multi-replica requirements are now **enforced**: when
`replicas > 1` but the spec has no config sync bucket, uses a filesystem
backend, or has no shared bootstrap password hash, the operator refuses to
scale up — a fresh deployment comes up with one pod, an already-running
fleet keeps its current size (healthy pods are never killed over a
preflight problem, and a transient Secret-read failure changes nothing) —
and the resource reports phase `Degraded` with the exact problems in
`status.message`. A broken spec can no longer scale into data corruption.
`spec.bootstrapPassword.autoGenerate: true` removes the manual hash step
entirely: the operator generates a random password once, stores it with
its bcrypt hash in a Secret named `<name>-bootstrap` (which deliberately
survives deletion of the resource, like the data volumes it decrypts),
and injects the hash into every pod. The operator image is published to
Docker Hub by CI on every operator version bump (skipped entirely when
the version is already published). Consistent hashing is the only multipart strategy
implemented — the trade-offs (ring remap on scale, per-pod upload loss on
restart) are documented in the operator README and in
`docs/product/how-to/scale-out-with-the-kubernetes-operator.md`. The Helm
chart remains the single-pod path and now says so explicitly.

The router stamps `X-Forwarded-For` (and the pods get
`DGP_TRUST_PROXY_HEADERS=true`) so rate limits, `aws:SourceIp` conditions,
and admin-session IP binding see real client IPs; ring membership tracks
`/_/ready` (real backend readiness), not just liveness. The Helm chart's
readiness probe now also points at `/_/ready` instead of `/_/health`,
matching the backend-health invariant shipped in v1.16.0.

## v1.16.1 — 2026-07-22

### Fixed — Verify no longer blames the scan cap for transient read failures

A partial Verify audit carried only one flag, so the UI always rendered the
"scan capped — raise `DGP_PARITY_MAX_OBJECTS`" advice even when the real cause
was a handful of objects whose reads kept failing transiently (backend
throttling) — raising the cap does nothing for that. The outcome now carries
the cause separately: a genuinely capped scan keeps the cap advice, while
unresolved reads render as "N objects couldn't be read — re-run the audit
(usually clears); if it persists, lower `DGP_PARITY_HEAD_CONCURRENCY`". The
server verdict sentence makes the same distinction.

## v1.16.0 — 2026-07-21

### Added — backend connection health is now enforced, end to end

A storage backend that can't be reached or whose credentials are rejected can
no longer hide (previously it booted silently and every request failed
one-by-one — a bucket on it even rendered as innocently "empty" in the
browser):

- **Boot probe.** Every configured backend (default + named) is probed at
  startup — an authenticated call, with a scoped-key fallback so
  bucket-restricted keys (e.g. Backblaze B2 application keys) don't
  false-alarm. Unhealthy backends are logged as ERRORs naming the cause
  (credentials rejected / endpoint unreachable / erroring). If **all**
  backends fail **definitively** (credentials rejected or unreachable — a
  merely-erroring/throttling backend never triggers this, so a provider
  hiccup can't cause a crash loop), the proxy **refuses to start**. Tunable
  via `DGP_BOOT_BACKEND_PROBE` = `enforce` (default) | `warn` | `off`.
- **Honest 503s.** Requests to a bucket routed to a backend with a
  *definitive* fault (credentials rejected / unreachable) answer a fast
  `503 ServiceUnavailable` naming the backend and cause — all verbs —
  instead of per-request timeout storms or misleading 404s. A reachable but
  erroring backend is flagged, never blocked. Unhealthy backends are
  re-probed every 30s, so recovery self-heals. The 503 (which names internal
  topology) is only sent to authenticated callers.
- **Apply gate.** Adding a backend or changing a backend's definition
  (endpoint, credentials) via the GUI or `config apply` now runs a live
  connection probe first — a typo'd secret or dead endpoint is **rejected
  with the exact cause** instead of going live silently. Unchanged backends
  are never re-probed.
- **GUI health.** Storage → Backends shows a live health badge per backend
  (Connected / Credentials rejected / Unreachable), a loud error card when
  one is down, and a **Test connection** button that runs the probe
  server-side — exercising the server's actual credentials (the old test
  ran from the browser and couldn't check credentials at all).
- **No more innocent empty states.** A failed listing in the file browser now
  renders as an error naming the cause ("bucket not found on its backend" /
  "bucket unavailable: backend X unreachable") instead of "No objects yet —
  add demo data". The bucket-usage chip stops showing stale counts for a
  bucket whose backend is unreachable.

### Fixed — misrouted buckets can no longer exist silently

A bucket routed to an **undefined backend name** was a warning ("route will be
ignored") and silently fell through to the default backend — where it 404'd.
It is now a **fatal config error**: the proxy refuses to boot with it, and
every apply / section save rejects it, naming the bucket, the missing backend,
and the available backends. Duplicate backend names are fatal for the same
reason. Additionally, a bare 404 on bucket probes (HEAD responses carry no
error body) was misread as a *transient* error, leaving an unrouted bucket
"transiently" unroutable forever and blocking backend discovery — now
correctly classified as "bucket absent".

### Fixed — GUI bucket edits no longer fail with a cryptic serde error

Editing a bucket's placement (or any storage setting) could fail with
`invalid storage section body: invalid type: null, expected a sequence` — a
raw serde error, unfixable from the GUI. Root cause: the server's JSON
merge-patch inserted `null` values verbatim for keys that didn't exist yet
(RFC 7396 defines null as *removal*, which is a no-op on an absent key).
Fixed per the RFC, and config errors now name the exact offending field
(`storage.buckets.<name>.public_prefixes: invalid type ...`) instead of a
path-less message.

### Changed — Verify scan-cap default raised to 1,000,000

The default `DGP_PARITY_MAX_OBJECTS` (max objects a Verify audit scans across
both sides before it caps and reports a partial result) is now **1,000,000**
(was 200,000). This is a runaway-scan safety ceiling — the audit already has a
throttle backstop and tunable concurrency — so the old default was capping
ordinary large mirrors (~100k objects/side) and reporting them as "not fully
verified" when they were fine. The new default covers ≈500k objects/side; still
overridable via the env var for even larger buckets.

## v1.15.5 — 2026-07-20

### Changed — Verify is now honest about what it proves (a metadata audit)

The replication **Verify** feature is now called an **Audit** and states plainly
what it does: a fast *metadata* audit that checks every source object exists on
the destination with matching **recorded** checksums and sizes — with **no
downloads**. It compares recorded metadata; it does not re-read the destination's
stored bytes, so it proves the two sides *agree*, not that the destination
reconstructs byte-for-byte. The old copy over-claimed a byte-level guarantee it
never delivered (the recorded checksum is copied source→dest verbatim on the
delta fast path, so a match there is a metadata match, not a proof of the landed
bytes). A clean audit now shows a **"Want byte-level certainty?"** panel that
names deep byte verification as a separate, upcoming capability — so the ceiling
of the audit is explicit, not implied.

We investigated a cheap "compare the backend ETag" shortcut to catch stored-byte
corruption without downloads and **rejected it**: it can't be gated safely across
encrypted, cross-backend, or multipart destinations without painting healthy
mirrors as corrupt. Real byte-level proof requires reconstructing and re-hashing,
which will ship as an explicit opt-in.

### Changed — one authoritative Verify verdict

The audit now returns a single server-decided verdict — **Verified in sync**,
**Not fully verified**, or **Differences found** — with a plain-language sentence,
instead of the screen inferring severity from raw counts. This removes cases where
the headline and the status icon could disagree (e.g. "685 differences" for a
backup where nothing was actually different).

### Fixed — Verify progress no longer looks frozen

On a large bucket the progress bar sat at an indeterminate "spinner" for the whole
listing phase (the slowest part), so a multi-minute audit looked stuck and got
killed. The bar now seeds an estimated total from the previous run's object count
(so it's determinate from the first update) and labels the phase ("Listing
objects…" → "Comparing…"); the exact total still corrects it once listing
finishes. The "scan capped" notice now says plainly that the rest is *unverified*
and names the `DGP_PARITY_MAX_OBJECTS` knob to raise the cap.

### Fixed — Verify no longer calls size-only matches "differences"

The **Verify** result counted objects that match on size but can't be
checksum-compared (no comparable checksum on both sides) toward its "N
differences found" headline, with an amber warning — so a backup where nothing
was actually wrong could read as "685 differences found". Those are matches, not
differences: the headline now counts only genuinely missing/extra/corrupt
objects, a size-only-only result reads **"No differences found"** with a neutral
info icon, and the explanation no longer asserts a single cause — it names both
(objects written to the backend directly instead of through the proxy, or the
two sides storing them differently) and states plainly that it is not a mismatch.

### Added — tunable Verify scan cap

`DGP_PARITY_MAX_OBJECTS` (default 200000) sets how many objects a Verify audit
scans across both sides before it caps and reports a partial ("scan capped")
result. Raise it for buckets larger than ~100k objects so the whole bucket is
covered in one audit.

## v1.15.4 — 2026-07-19

### Fixed — much faster replication Verify

The parity **Verify** audit on large S3↔S3 rules crawled (a ~99K-object backup
took ~30 minutes) because it checked objects in small batches, waiting for each
batch to finish before starting the next, and re-checked the tiny `.sha1`/
`.sha256`/`.sha512` checksum files even though it already knew their size. Verify
now streams the checks continuously and skips the checksum sidecars, so it
finishes several times faster. Concurrency is tunable via
`DGP_PARITY_HEAD_CONCURRENCY` (default 15) for cautious dialing on a slow backend;
the backend-throttle backstop is preserved.

### Fixed — Verify no longer shows a stale result mid-scan

While a new Verify was running, the screen could show the **previous** scan's
findings (with an old timestamp) as if they were current — which looked like the
running scan had already found problems. A running/cancelling Verify now shows
live progress instead of a phantom verdict.

### Fixed — Verify could report a stale size after an overwrite

On a filesystem destination, Verify's per-object cache keyed on the file's
logical checksum, which doesn't change when the same file is re-compressed
against a rotated baseline — so an overwritten object could be reported with its
**old** size. The cache now keys on the stored blob's own version, so an
overwrite is always detected.

### Changed — "Re-run won't help" is now "Re-run may help" for transient errors

A copy that failed on a **transient** error (a slow/stalled read, a timeout, a
throttle, a dropped connection) was shown as "Re-run won't help", discouraging a
retry that would in fact succeed. Those now read **"Re-run may help"**; only a
structural failure (permissions, missing bucket) stays a hard no.

### Changed — delete replication is a faithful mirror on every path

Delete replication now removes any destination object absent at source
regardless of provenance on **both** the scheduled reconcile and the
event-driven path (previously the event-driven path still preserved objects it
hadn't written itself, so the two paths could delete differently).

## v1.15.3 — 2026-07-18

### Fixed — false "checksum mismatch" on replicated delta objects (+ a silent corruption)

When a destination backend dropped a delta object's DeltaGlider metadata headers
(the same stripping that affected references), two things went wrong: the
post-copy check compared the source's real size against the destination's
*compressed delta* size and reported a bogus "truncation" that a re-run could
never clear; and — more seriously — a download of such an object served the raw
delta bytes instead of reconstructing the original (a corrupt file under a
success response). Both are fixed: the check now recognises the stripped-delta
case, and the copy **re-stamps the missing metadata** so the object stays
downloadable and correct.

### Changed — delete replication is now a faithful mirror

With delete replication enabled, the destination is now a faithful copy of the
source: **any** destination object not present at source is removed — including
objects written by other tools. Previously only objects this exact rule had
written were removed, so a destination could never fully converge. The
destination bucket must be **dedicated to the rule**; a bucket shared with
another writer is no longer a supported setup.

## v1.15.2 — 2026-07-18

### Added — each run shows which algorithm it applied

The run drawer now names how each copied object was moved, in plain language:

- **⚡ shipped as-is** — the delta bytes were copied verbatim, no decompress or
  recompress (the cheapest path; hover for the technical term).
- **↻ rebuilt** — decompressed from the delta and re-stored (recompressed or
  re-encrypted) at the destination.
- **→ straight copy** — a whole already-compressed object (image, video,
  archive) copied byte-for-byte.

Alongside the mix, **"saved &lt;N&gt;"** reports the egress that never crossed the
wire because deltas shipped as-is. This turns the run history into an honest
account of what the engine did, not just how many objects moved.

## v1.15.1 — 2026-07-18

### Fixed — replication no longer re-copies destinations with stripped metadata

A destination bucket whose objects were copied in with a tool that didn't
preserve S3 metadata (the DeltaGlider `x-amz-meta-dg-*` headers) would be
**re-replicated in full on every run** — the proxy couldn't read the
destination's logical size/hash, so a content-diff rule always saw a
"difference" and re-shipped, potentially gigabytes per run, forever.

- **The destination now self-heals on write.** When a replication copy lands in
  a prefix whose reference lost its metadata, the proxy re-stamps that metadata
  in place (the stored bytes are never touched, so nothing already there
  breaks). After one more pass the destination reads cleanly and the rule
  **converges** — byte-identical objects are skipped instead of re-copied. This
  also restores the destination as a usable backup (delta reconstruction needs
  that metadata).
- This affected pre-existing corrupt destinations regardless of version; it was
  never introduced by a release.

### Added — live reconcile progress in the Jobs screen

A running replication reconcile now shows what it's doing: the run drawer
displays **"scanning `<current folder>` · N folders done · M pending"** with a
live activity bar. A long comparison pass that is skipping already-replicated
objects (and therefore shows "0 copied") now reads as *working, and here* rather
than looking stuck.

## v1.15.0 — 2026-07-17

### Changed — replication reconciliation rebuilt as a streaming tree walk

The scheduled reconcile pass no longer spends hours "initializing" on large
buckets before copying anything. The old three-phase design (a full dual-tree
discovery pre-pass, then a flat copy sweep, then a third full destination
listing for deletes) is replaced by ONE directory-by-directory walk that
syncs each folder as it discovers it.

- **First copies start within seconds** of a run starting, regardless of tree
  size — discovery and copying are interleaved, with configurable parallel
  directory listings (`replication.dir_concurrency`, default 4).
- **Far fewer requests against the backend.** Listings run in lite mode
  everywhere (the per-page HEAD-enrichment sweeps are gone); a subtree missing
  on the destination is proven absent from the folder comparison itself, so
  its objects copy with zero existence probes. On a filesystem↔filesystem
  pair, an entire run — initial sync or converged re-check — issues **zero**
  per-object HEADs; on S3 backends only keys present on BOTH sides are
  HEAD-resolved, batched.
- **Delete replication is now per-directory.** A folder that reconciled
  cleanly can replicate its deletions even when the run stops early elsewhere
  (the old design only deleted after a perfect full pass), while keeping every
  safety layer: provenance marker, HEAD re-confirmation of source absence,
  and never touching foreign objects.
- **Resume is finer-grained.** The persisted cursor is now a position in the
  tree (scope-stamped to the rule's buckets/prefixes); a killed, paused, or
  budget-truncated run resumes exactly where it stopped, even mid-directory.
  On upgrade, the previous cursor format is discarded once and the next run
  starts a fresh (idempotent) pass.
- **Live progress**: the Jobs screen shows directories completed/pending for
  a running reconcile, and three new Prometheus counters
  (`deltaglider_replication_list_calls_total`, `..._head_calls_total`,
  `..._dirs_completed_total`) make the walk's I/O auditable.
- The walk core is a pure state machine with a model-based property-test
  suite (action-set equivalence against a naive global diff, crash-cut resume
  equivalence, page-size invariance, never-false-delete, cursor monotonicity,
  budget convergence, degrade-mode equivalence).

### Fixed

- **Kill/pause/lease-loss now interrupt a replication run promptly at any
  phase.** Two driver bugs found while stress-looping the integration suite:
  control checks could go dead for the remainder of a run after listings
  finished, and a config-DB lock could be handed to a suspended poll future,
  freezing every DB user in the proxy (jobs API, heartbeat, kill) until
  restart.
- **Filesystem delimiter listings no longer hide dot-directories** (e.g.
  `.well-known/`): they were invisible as folders while their objects were
  reachable directly — now consistent with flat listings and the S3 backend.
  Dot-files (internal temp namespace) stay hidden; `.dg` stays internal.
- **Keys with an empty path segment (`a//x`) now replicate to their true
  destination key.** The per-object rewrite used by the old pass collapsed
  the doubled slash (`a//x` and `a/x` fought over one destination key); the
  walk maps prefixes verbatim. Pre-existing collapsed copies become
  rule-owned orphans that a delete-enabled rule converges away.

## v1.14.1 — 2026-07-16

Two operability fixes from production issue reports.

### Fixed

- **`/_/ready` no longer flips to a paging 503 on a brief backend blip** (#62).
  The readiness probe's backend check is now retried with a tunable per-attempt
  timeout (`DGP_READY_TIMEOUT_SECS`, default 3) and retry count
  (`DGP_READY_RETRIES`, default 2) — it reports not-ready only if every attempt
  fails, so a short storage-provider latency spike (e.g. object-storage tail
  latency) doesn't mark an otherwise-healthy edge out of rotation. Point load
  balancer **readiness** checks at `/_/ready` and **liveness** at `/_/health`.
- **A bucket declared in `storage.buckets` on a filesystem backend is created at
  startup** (#63), so its first write no longer returns `NoSuchBucket` before an
  explicit `CreateBucket`. Declared intent only: implicit bucket creation on the
  write path stays refused, and remote S3 buckets are never auto-created.

## v1.14.0 — 2026-07-10

The completion of the correctness/security review begun in v1.13.0 — the
remaining edge-case findings, all fixed with regression tests. Multi-instance
deployments and shared-host installs get the most benefit.

### Fixed — data & integrity

- **Multipart uploads verify each part's content at completion**, so a part file
  tampered with on disk (shared-host attack) is rejected instead of stored.
- **A streamed upload with an unexpected size is rejected** instead of stored
  short, and a garbled backend error can no longer crash the notification
  dispatcher.
- **A deleted object can't be resurrected** by a leftover storage variant, and an
  object addressed with an extra leading slash shares one cache entry.
- **A backend returning HTTP 429** (Backblaze B2, Cloudflare R2) is treated as a
  transient throttle, not a hard error.

### Fixed — multi-instance / jobs

- **Run-now, verify, and delete on a replication rule now see a run in progress
  on another node** (when cluster coordination is on), preventing double-runs and
  deleting a rule mid-run.
- **A stuck replication consumer no longer makes the event log grow forever**, and
  a fully-throttled small batch now backs off instead of grinding.
- **A killed replication run no longer mis-marks a successfully-copied object** as
  failed.
- **A migration's source cleanup no longer stops early** when it hits a page of
  folder markers, and a concurrent large upload's parts aren't swept by the
  periodic cleanup.

### Fixed — config & correctness

- **Values containing `$` round-trip correctly** through save/reload, applying a
  redacted export can't create a passwordless user or provider, and editing a
  masked URL list won't restore the wrong secret.
- **Deleting an OAuth provider on one node removes it everywhere**, and streaming
  transfers account for their temporary disk so heavy concurrency can't exhaust
  it.
- Several smaller hardening fixes (codec watchdog false-kills, backend recovery
  after a cancelled probe, parity conflict timestamps).

## v1.13.0 — 2026-07-10

A correctness-and-security hardening release. A deep review of the codebase
turned up a set of data-loss, data-corruption, and authorization-bypass bugs
in edge cases (concurrent config edits, cancelled uploads, slow backends,
rejected config applies); every one is fixed with a regression test.

### Fixed — data loss & corruption

- **Concurrent admin edits no longer silently lose a user.** Creating two
  users in quick succession could make one of them vanish everywhere (the
  config-DB sync path clobbered a just-committed change with an older copy).
  Same-node syncs are now serialized.
- **A slow upload to a remote backend no longer loses its data.** A large
  multipart upload whose final store ran longer than the cleanup timeout could
  have its temporary parts deleted mid-store, permanently failing the upload on
  retry. In-flight stores are now protected from the cleanup sweeper.
- **Background copies no longer race a re-encryption/migration job.** A
  replication or lifecycle copy in flight when a maintenance job started could
  be overwritten with stale bytes (or lost after a migration flip). Those
  copies now register with the maintenance write-gate so the job waits for them.
- **A dropped connection no longer wedges a bucket's maintenance jobs.** A
  client disconnecting mid-write used to leak an internal counter, making every
  later re-encrypt/migrate on that bucket time out until a restart. The counter
  is now released even on cancellation.
- **A just-created bucket appears immediately in the next listing** even under
  a concurrent listing refresh (a race could hide it for a few seconds).
- **A backend's encryption key survives a config export→edit→apply round-trip**
  — it was being silently stripped, which could leave historical objects
  unreadable and new writes unencrypted.

### Fixed — security & authorization

- **A rejected config apply no longer leaks a public prefix.** An apply that
  failed its IAM validation could still publish a public-read prefix (serving
  objects anonymously) while reporting "no change." Validation now runs before
  any change is committed.
- **Prefix-scoped and IP-scoped Deny rules are enforced on batch deletes and
  browser (form-POST) uploads** — both paths previously skipped the per-object
  authorization check, defeating carve-out and source-IP conditions.
- **A revoked session is dropped from memory immediately**, and a compromised
  key can no longer be un-revoked by a concurrent config sync.
- **A quota can no longer be bypassed** by a corrupt object-size reading that
  overflowed the usage total, and a truncated streaming upload is now rejected
  instead of stored as a complete (short) object.

- **A run whose process died within its lease TTL no longer shows "running"
  forever.** The boot-time zombie scan spared any `running` row whose leader
  lease still looked live — but a process that dies right before a restart
  leaves exactly such a lease behind, so its run dodged the (boot-only) scan
  permanently. Boot now lapses every local lease first: the coordination
  tables are node-local, so a lease found at boot can only belong to a dead
  process of this node. Applies to replication and lifecycle.

### Fixed — config apply

- **Changes to passthrough size limit, codec concurrency, TLS, and the config
  sync bucket now take effect (or clearly warn a restart is required)** instead
  of being reported as applied while silently doing nothing.

### Fixed — additional hardening

- **A backend rate-limiting with HTTP 429** (Backblaze B2, Cloudflare R2) is now
  treated as a transient throttle (back off and retry) instead of a permanent
  500.
- **The event-notification dispatcher no longer crashes** on a backend error
  message that happened to contain a multi-byte character at a truncation point.
- **Applying a redacted config export to a fresh instance** now fails clearly if
  a new user or OIDC provider has no secret, instead of creating identities that
  can never authenticate.
- **Objects addressed with an extra leading slash** (`/key` vs `key`) no longer
  leave stale cached metadata after a delete.
- **Streaming delta transfers account for their temporary disk correctly**, so
  many concurrent large transfers can't exhaust the spool area.
- Editing a masked webhook/Slack URL list no longer risks restoring the wrong
  saved secret, and internal object keys no longer trigger user webhooks.

## v1.12.4 — 2026-07-09

The "plays nice with throttled backends" release: batched HEAD sweeps with
circuit breakers, adaptive client-side rate limiting, replication runs that
back off instead of grinding a throttled key list — and a job drawer that
shows WHAT is copying (name + size), so a slow counter reads as honest work.

### Fixed

- **HEAD sweeps stop instead of grinding a throttled backend.** Listing
  enrichment now runs its per-object HEAD lookups in batches of 10 (down
  from 50 concurrent) and aborts the sweep when a batch comes back mostly
  `503 SlowDown` — the remaining objects render with their listing
  metadata. Deltaspace savings scans keep full coverage but log throttle
  counts.
- **The job drawer shows WHAT a replication run is copying.** An active
  run now lists its in-flight objects (name, size, started) above the
  tabs — a counter sitting on "1 copied" for 30 minutes reads as honest
  work on a 4 GB tarball instead of a hang. Live view, top 3 largest,
  refreshed by the existing 2s poll.
- **Background jobs stop hammering a throttled backend.** A replication
  run now aborts (and backs off to the rule's cadence) when a whole page
  of copies is rejected with `503 SlowDown`, instead of grinding the
  remaining key list — the cursor resumes where it left off. Verify's
  HEAD-resolution runs in batches of 10 (down from 50 concurrent) and
  marks the remaining keys unresolved (honest partial audit) when a whole
  batch fails transiently.
- **The S3 client uses adaptive retry.** One throttle response now
  rate-limits every subsequent request on that backend client (HEAD
  bursts, listings, replication reads) via the AWS SDK's client-wide
  adaptive rate limiter, instead of each call site independently
  hammering a struggling backend.

## v1.12.3 — 2026-07-09

A network-hygiene patch, prompted by a Hetzner `SlowDown` throttling episode
whose RCA surfaced how chatty a bucket refresh really was.

### Fixed

- **One upstream `ListBuckets` per backend per refresh, not two.** The browser
  fires the S3 bucket list and the backend-origins lookup back-to-back; the
  routing layer now serves a listing fetched within the last 5 seconds without
  re-probing upstream (`DGP_BACKEND_LIST_FRESH_SECS`, `0` disables). Creating
  or deleting a bucket through the proxy invalidates the window immediately,
  so read-after-write stays exact. Stale rosters still back the
  unavailable-placeholder path during outages.
- **Five UI components no longer fire five independent bucket listings.** The
  sidebar, destination picker, analytics, scan card, and permission editor now
  share one in-flight request and a short-lived result; the explicit refresh
  button and bucket create/delete bypass the cache.
- **Alt-tabbing back no longer bursts the admin API.** React Query's
  focus-refetch re-fired every active query on each window focus; it's off —
  live data is covered by the explicit pollers.
- **The idle bucket-maintenance poll runs 4× less often** (60s instead of 15s
  when no job is running; still 2s while one is active).

## v1.12.2 — 2026-07-09

The "feels alive" release: the golden-roadmap UX waves (verify progress that
never looks dead, first-press Back button, deep-linkable admin views, plainer
copy, a virtualized object table), a listing memory fix, community-contributed
deny-path metrics + an IAM secret-preservation fix, and the three maintenance
commits that accumulated after v1.12.1 (SigV4 single-verification, dead-backend
listing cooldown, replication-target alias hardening).


### Added

- **Every admin view is now deep-linkable.** The job drawer, metrics view
  toggle, setup wizard step, file preview, user/group selections, and all
  modals (YAML import/export, full-IAM, inspector, destination picker) push
  their state to the URL query string. Share a link, bookmark it, or use
  Back/Forward — the UI restores to the right place. Modals close on Back
  button; direct-loaded deep links open the overlay and close safely without
  walking out of the SPA.

- **Denied requests now show up in Prometheus.** Admission-rule and IAM
  authorization 403s short-circuit before the metrics middleware, so they were
  invisible to `deltaglider_http_requests_total`. Both deny paths now record
  the sanitized counter (method/status/operation). Contributed by @skel84.
- **The object table is virtualized.** Only the visible rows render in the
  DOM (previously up to 500), the body height fits the viewport, and the
  pagination bar stays visible. Keyboard navigation scrolls via the table's
  native `scrollTo`.
- **Loading states show motion everywhere.** Bare "Loading…" labels in the
  webhook, audit, buckets, outbox, and dashboard panels are replaced with the
  shared spinner/skeleton primitive.

### Fixed

- **Verify progress bar no longer looks dead on large scans.** A four-part
  fix: the server now flushes parity progress on a 250ms wall-clock throttle
  (not just per-page), the UI polls verify status at 800ms (not the generic
  2s), the first progress frame is no longer stuck on "Starting scan…", and
  a reusable `useTweenedCount` hook adds a smooth count-up animation that
  honors `prefers-reduced-motion`.
- **First browser Back no longer swallowed.** The SPA router's `skipNext`
  flag was armed on every `pushState` but `pushState` never emits `popstate`,
  so the flag sat armed and ate the first real Back press. The flag is
  removed entirely — Back works on the first press.
- **Login no longer double-fires on Enter.** A synchronous `useRef` guard
  replaces the async `useState` check, so `onPressEnter` and the button
  `onClick` in the same tick can't both pass the guard.
- **Renaming a job no longer spams browser history.** The drawer's rename
  retarget now uses `history.replaceState` instead of `pushState`, so typing
  5 characters doesn't create 5 Back entries.
- **Invalid `?tab=` on a shared job link no longer renders a blank drawer.**
  The tab is clamped to the tabs that exist for that job kind.
- **Inspector close on a direct-loaded `?object=` link no longer walks out
  of the SPA.** The close handler now uses the direct-load-safe overlay
  pattern: `history.back()` if we pushed the entry, `replaceState` if it
  was loaded from a shared link.
- **Setup wizard no longer offers "Save and start" over empty config after
  a refresh.** The step is forced to 0 when form state is initial; the
  `?step=` deep-link only applies to in-session Back/Forward.
- **Savings chip no longer hides genuine sub-1% savings.** The zero-guard
  now checks the raw float from the server, not the integer-floored display
  value.
- **Hidden tabs no longer waste network requests.** Two unguarded 30s
  `setInterval` pollers (bucket scan card, analytics section) are routed
  through `useVisiblePolling`, which stops polling on hidden tabs and fires
  an immediate refresh on tab return.

- **Delimiter-less LIST no longer materializes the whole prefix.** A
  ListObjectsV2 without a delimiter fetched every key under the prefix into
  memory before cutting one page out of it. The S3 backend now pages natively
  with a provably-safe early exit (the completeness horizon accounts for
  `.delta`-suffix collation — prefix-sharing names like `v1`/`v1.2` were the
  hard case, validated by brute-force simulation).
- **Applying an exported config no longer wipes IAM user secrets.** The
  document export deliberately redacts `secret_access_key` to an empty
  string; the declarative reconciler treated that as a rotation to an empty
  secret and broke every user's credentials. Empty now means "keep the
  existing secret", matching the field- and section-level admin paths.
  Contributed by @skel84.
- **The bootstrap login hint speaks plainly.** "Deployment-level access for
  first run and recovery" → "For first-time setup and recovery".
- **Replication-target buckets can't be written via their unconfigured alias.**
  A client PUT to a marked bucket's real (alias) name — the one discovered by
  cross-backend head-probe, not the configured virtual name — bypassed the
  single-writer guarantee. The proxy now indexes every marked bucket's real
  storage name and blocks writes that resolve onto protected storage, while
  still exempting names with their own explicit backend route.

- **A dead backend no longer hangs bucket listings.** After the parallel
  fan-out, one unreachable backend still cost a connect timeout (up to 10s) on
  every ListBuckets refresh. A per-backend failure cooldown
  (`DGP_BACKEND_LIST_COOLDOWN_SECS`, default 30) now skips a backend that just
  failed a listing — serving from last-known-good, flagged unavailable — and
  each listing call is bounded by `DGP_BACKEND_LIST_TIMEOUT_SECS` (default 5).

### Changed

- **Clearer labels throughout the admin UI.** "Bootstrap password" → "Admin
  password", "Storage footprint" → "Storage used", "Trace" → "Rule tester",
  "Delta efficiency" → "Compression health", "Admission rules" → "Request
  rules", "Event outbox" → "Event log". Six verbose verify descriptions
  replaced with plain language. Rust denial messages softened to
  human-readable text. Product docs updated to match.
- **Natural sort everywhere.** Bucket names, object keys, and rule lists now
  sort naturally (`v2` before `v10`) via a shared `numericCompare` helper.
- **Consistent error display.** All 58 `e instanceof Error` sites across 25
  files replaced with a shared `normalizeUiError` helper.
- **Config warnings use human-readable text.** `Config::check()` warnings
  no longer leak the internal field name `replication_target_only`.
- **SigV4 is verified once, not twice.** The redundant outer axum signature
  verifier (~380 LOC of hand-rolled canonical-request + HMAC) is deleted; s3s
  is now the sole SigV4 authority. The outer middleware resolves identity only
  (access-key → user) and keeps replay detection, rate-limiting, and anonymous
  public-prefix minting. s3s rejects forged signatures — header, presigned, and
  chunked streaming — before any handler runs. A valid-AKID/wrong-secret
  forgery is still 403'd but no longer feeds the outer brute-force lockout
  (HMAC-guessing is infeasible; locking on it would let an attacker DoS a
  victim's key). Credential-enumeration lockout is unchanged.

## v1.12.1 — 2026-07-08

A hardening release from a full adversarial review of the last several versions.
No new features; correctness and safety fixes throughout.

### Fixed

- **Verify no longer flags a healthy mirror as broken.** On same-backend
  mirrors, some correctly-replicated objects could show up as a permanent
  "checksum mismatch" that no re-run could clear. Verify now compares the same
  logical checksum in every case (reading it straight from the listing when the
  backend allows, so it stays download-free), and the progress bar climbs
  smoothly to 100% instead of overshooting or freezing.
- **A temporarily-unreachable backend no longer hides buckets.** Buckets on a
  backend that's throttling or down now stay listed (flagged unavailable) even
  when they aren't explicitly pinned to that backend, and the bucket list
  survives a full outage instead of collapsing to an error. A throttled backend
  now returns a proper "slow down" signal so clients back off, rather than an
  error clients treat as permanent. The UI also stops trying to browse, scan,
  or copy into buckets it knows are unavailable.
- **Storage migration can't strand the proxy.** Migrating a bucket onto a
  backend that doesn't support the required safety guarantees is now refused up
  front, instead of succeeding and then failing to start on the next restart.
- **Backend capability detection is more robust.** The check that decides
  whether a backend is safe for multi-instance use no longer misreads unrelated
  details (an endpoint port, a request id) as a failure, and never leaves a
  stray probe object behind in your buckets.
- **Browser (form) uploads honor the full upload policy.** An upload whose
  signed policy doesn't constrain the object key is now rejected, matching AWS
  behavior, so a leaked upload signature can't be reused to write elsewhere.
- **Replication mirrors are protected from accidental deletion.** A
  replication-target bucket can no longer be created or deleted through the S3
  API by clients.

### Changed

- Faster bucket listings when multiple backends are configured (queried in
  parallel), and clearer error detail when a backend can't be reached.

## v1.12.0 — 2026-07-06

Verifying a replication mirror now shows real motion, and a pure 1:1 mirror
verifies without downloading anything.

### Added

- **Verify shows live progress.** While a parity recompute runs, the panel now
  shows a scanned-object count that ticks steadily plus an objects/sec rate, and
  a determinate progress bar once the total is known — instead of a spinner that
  gave no sense of movement.
- **Pure mirrors verify with zero downloads.** When source and destination use
  the same encryption and compression (a plain 1:1 mirror), Verify now compares
  the stored object size and etag straight from the bucket listing — no
  per-object metadata fetches. The verdict reads honestly as "exact copies (same
  stored size + etag)", and the footnote says the check used the listing, not a
  checksum download.

### Changed

- **Verify picks its strategy per rule.** A mirror that re-encrypts,
  re-compresses, or re-deltas at the destination (so the stored bytes
  legitimately differ) still uses the full logical-checksum comparison — that
  path is unchanged. Only genuine byte-for-byte mirrors take the cheap
  listing-only path, so an encrypted or transformed destination is never falsely
  flagged as mismatched.

## v1.11.0 — 2026-07-05

The bucket list never lies about what exists when a backend is unreachable.

### Fixed

- **A temporarily-unreachable backend no longer empties or silently truncates
  the bucket list.** When a storage backend returns errors (a provider
  503-throttling, a connection failure), its buckets used to vanish from the
  list — and if every backend was erroring, the list came back empty, which the
  UI showed as "create your first bucket" with a clean console. Now: if *all*
  backends fail, the listing returns an error (the UI shows a distinct
  "couldn't load buckets" state with retry, not an empty one); if *some* fail,
  the reachable buckets are still listed.

### Added

- **Unreachable buckets stay visible, flagged and explained.** A bucket whose
  backend is temporarily down is now listed (greyed, non-clickable, with an
  "unavailable" badge) instead of being dropped — sourced from the configured
  routing table so the roster stays complete. Hovering it shows the **verbatim
  backend error** (e.g. the raw `503 SlowDown`), so operators see exactly why a
  bucket is dark rather than assuming it disappeared.

## v1.10.0 — 2026-07-04

Safe non-CAS backends (Backblaze B2 as a cheap replication target), automatic
replication failover across instances, and a hardened browser upload path.

### Added

- **`replication_target_only` bucket marker.** Mark a bucket a
  replication-destination-only mirror: client writes (PUT, DELETE, multipart,
  copy-into, browser uploads, admin bulk copy/move/delete) are refused with
  403 so replication stays the single writer. This is what makes a backend
  without conditional-write support (e.g. Backblaze B2) safe to host a mirror —
  reads are unaffected, and prefixes can still be published read-only. See the
  new how-to, [Using non-CAS backends safely](https://deltaglider.com/docs/how-to/backend-capability-validation).
- **Backend conditional-write validation.** When multi-instance mode is active
  (a `config_sync_bucket` is set), the proxy probes every S3 backend that hosts
  a client-writable bucket at startup and refuses to boot on one that doesn't
  enforce conditional writes (a data-corruption trap under concurrent writes).
  A runtime `/config/apply` that would create the same situation is rejected.
  Verdicts surface in the admin GUI's Backends panel; every enforcement message
  links to the troubleshooting doc.
- **Automatic replication failover across instances.** With a CAS-capable
  coordination bucket, replication rules elect a single leader per rule via an
  S3 lease object; a dead leader's lease lapses and a peer takes over
  automatically — no double-run, no shared database required. Single-instance
  deployments keep the node-local lease (zero S3 traffic).

### Fixed

- **Hardened the browser form-POST upload path.** Closed a bucket-name path
  traversal (an unvalidated bucket segment could escape the storage root),
  routed failed upload signatures through the same per-IP brute-force limiter
  and audit ring as every other auth surface, and now reject form fields not
  covered by the signed policy (AWS's "every field must have a condition"
  rule). The signature/policy verification itself was audited clean.

### Docs

- New troubleshooting guide for backend capability validation; corrected stale
  HA/multi-instance claims across the README, product docs, and code comments
  to reflect the shipped cross-instance replication lease.

## v1.9.6 — 2026-07-03

Verify reliability + a Jobs-table icon fix.

### Fixed

- **A crashed Verify audit no longer shows "running" forever.** A verify
  (parity) audit runs as a background task that settles its own status when it
  finishes. If the proxy was killed mid-audit (or a re-launched audit crashed
  again) with no restart afterward, the row stayed stuck on `running` — the
  Jobs → Verify tab polled a spinner indefinitely, which looked like an
  extremely slow scan. The status read now self-heals: a `running` row whose
  background lease has expired (a live audit renews it continuously) is
  recognised as dead and settled to `failed`, without needing a restart.
- **Distinct icon for "Run now" vs "Resume".** After the Jobs actions went
  icon-only, a paused rule showed two identical play carets (Resume and Run
  now). Run now now uses a circled-play icon.

## v1.9.5 — 2026-07-02

Jobs-screen usability fixes.

### Fixed

- **Replication and lifecycle glob fields accept multiple lines again.** The
  Include/Exclude glob editors silently ate the Enter key — a controlled
  round-trip stripped the trailing blank line on every keystroke, so you could
  never start a second line. They now hold the raw text while you edit and only
  normalise on blur.
- **The Jobs table is cleaner and responsive.** Removed the segmented per-cell
  underlines (one row separator now), stopped the Scope and rule-name columns
  from wrapping (single-line with a full-text tooltip), and made the row actions
  icon-only so a job row no longer overflows; it collapses to readable cards on
  narrow screens.
- **The Verify "Differences" panel no longer repeats itself.** The WHY column
  showed the same cause twice (a wrapping amber block plus a duplicate grey
  line); it's now one compact verdict chip and a single cause line. The FIX
  column's guidance no longer looks like a clickable button.
- **The "New job" menu closes when you pick a type**, and opening a draft rule
  that was already discarded or renamed closes cleanly instead of flashing
  "this job no longer exists".

## v1.9.4 — 2026-07-02

Admin-UI simplification, a leaner CI pipeline, and wider property-test coverage.

### Changed

- **Admin pages simplified to their essentials.** Config panels now share two
  content widths instead of a dozen ad-hoc ones; the System "Request limits"
  fields dropped their per-field restart badges and environment-variable boxes
  for a single help line; the "data lives in the encrypted DB, not YAML" banner
  on Users/Groups/External-auth shrank from a three-fact block to one quiet
  line; Credentials and Backends copy is plainer with YAML keys as hover chips.
- **Dashboard leads with headline KPIs.** The Monitoring view shows 9 always-
  useful cards up front and tucks 11 deep-telemetry charts behind a default-
  closed "Detailed telemetry" toggle, so a fresh install no longer opens onto a
  wall of empty graphs.
- Removed duplicate in-panel headers on Event delivery, Trace, and Audit log
  (the page title already names them).

### Internal

- **CI is faster and leaner.** The UI bundle and release binary are now built
  once and shared as artifacts instead of being rebuilt in ~8 and ~3 jobs; the
  non-gating coverage and supply-chain (deny/audit) jobs moved to the nightly
  workflow, so the PR gate is correctness-only. E2E, docs, and config-schema
  jobs dropped several minutes each.
- **Wider property-test coverage** on the pure validators — bucket-key paths,
  the IAM method→action classifier (with a security invariant that mutating
  methods never map to read-only actions), public-prefix normalization, and
  replication key rewriting.
- Documentation refreshed to match the current admin UI and corrected config
  defaults (`DGP_CACHE_MB` 100 MB, `DGP_CODEC_CONCURRENCY` num_cpus × 4).

## v1.9.3 — 2026-07-01

Security + data-integrity fixes from an adversarial code review, plus the
non-blocking run-now the copyHzToBackup incident called for.

### Fixed

- **Config-DB recovery no longer destroys the IAM database (data loss).** After a
  bootstrap-password mismatch, a second unattended restart could overwrite the
  preserved backup (`deltaglider_config.db.bak`) with the empty database created
  on the mismatch boot — permanently losing IAM users, groups, and OAuth
  providers. The boot backup now refuses to clobber an existing `.bak`, and
  recovery is read-only (it returns the correct hash for you to set, rather than
  writing hidden state that led into the loop).
- **The bootstrap-hash sidecar is created private (0600) in one step.** It was
  written world-readable then chmod-ed, leaving a brief window in which a local
  user could read the SQLCipher key on a shared host.
- **Event-outbox "purge failed" can no longer drop unconsumed replication events.**
  The outbox is a shared stream that event-driven replication also reads; the
  failed-row pruner and the operator purge now respect the replication cursor
  floor (the purge refuses with a clear message when failed rows still sit above
  it), and failed rows age out even when delivered-retention is 0.
- **Killing a run is settled reliably.** A kill arriving in the gap between the
  final cancel check and the run settling could be overwritten by a "succeeded"
  status; the check and the settle now happen under one lock.
- **Deleting a replication rule with a run in progress is refused** (409 "kill it
  first") instead of purging the running worker's state out from under it.
- **`run-now` returns immediately (202) instead of hanging the request** for the
  whole replication — a multi-GB sync no longer blocks the admin call. Progress
  shows in the jobs list. `run-now` also runs a disabled or paused rule as a
  deliberate one-off (labelled "Run once") without re-enabling it.

### Added

- **Cross-instance session revocation.** Force-logging-out a user (the
  compromised-key escape hatch) now takes effect on every instance, not just the
  one serving the request — a synced revocation table is consulted on every auth
  check. (Killing a running replication job remains instance-local under a
  non-sticky load balancer — the UI/response now say so.)

## v1.9.2 — 2026-07-01

Replication performance + the kill/responsiveness fixes that an
empty-destination copy exposed in production.

### Added

- **Directory-aware replication (prefix-tree destination oracle).** Replication
  now descends the destination and source prefix trees like directories instead
  of flat-listing every key. A destination subtree that doesn't exist is proven
  absent from a single common-prefix probe, so its entire source subtree is
  bulk-copied with **zero per-object destination HEAD requests**. A copy to an
  empty or sparse destination no longer spends minutes "thinking" before it
  starts copying — an empty destination costs one list call. `SkipIfDestExists`
  rules now decide from the listing alone and never issue a per-object HEAD, even
  against a fully-populated destination.

### Fixed

- **Copy to an empty/sparse destination was pathologically slow.** The planner
  issued one destination HEAD per source object just to discover the object was
  absent (it always copies when missing, under every conflict policy) — minutes
  of wasted round-trips against a remote backend. Replication now lists the
  destination once and only HEADs keys that actually exist.
- **A running replication job could not be killed while it was planning.** The
  kill check only ran during the copy phase, so a run wedged in the per-page
  list/plan phase ignored the operator's Kill until it reached a copy
  checkpoint. The kill is now honored at every page boundary.
- **A killed run kept showing as "running" and a second Kill returned 409.** The
  Jobs list read the rule's last-finished status while Kill flips the in-flight
  run-history row to "cancelling" immediately — so a killed-but-not-yet-settled
  run is now correctly shown as "cancelling" instead of a stale "running".

## v1.9.1 — 2026-06-30

A hardening release for the v1.9.0 replication job controls (kill/delete), plus
three operator recovery hatches surfaced by an adversarial review.

### Fixed

- **Killing a run no longer leaks a multipart upload.** A killed replication run
  dropped the in-flight copy future without aborting the destination multipart
  upload, leaving it dangling on backends that don't garbage-collect incomplete
  uploads (e.g. Backblaze B2). The abort now runs on the cancellation path too.
- **A cancelled run is settled authoritatively.** A kill requested while the last
  page was draining could be overwritten by a success status; and a crash between
  the kill and the run settling left the row stuck `cancelling` forever. The
  `cancelling` state is now honored at settle time and reconciled at boot.
- **Deleting a replication/lifecycle rule can't resurrect it.** A persist failure
  during delete left the in-memory and on-disk config out of sync, so the deleted
  rule reappeared on the next restart. Delete now rolls back cleanly on failure,
  skips a needless engine rebuild, and purges the rule's DB rows in one
  transaction.
- **Fail-fast destination detection now matches real failures.** The "destination
  unusable" fast-abort missed the actual Backblaze B2 over-cap / dead-credential
  error shapes and could over-eagerly abort a healthy run on a stray token. It
  now recognizes the real shapes, only aborts when an entire page copies nothing,
  and backs off to the rule's normal cadence instead of re-firing every minute.
- **Lifecycle rules no longer show a non-functional Kill button** in the Jobs UI
  (kill is replication-only); killing a run now asks for confirmation.

### Added

- **Admin session revocation.** A new Access › Sessions page lists live admin
  sessions and lets an admin force-logout a single session or every session of a
  given access key — the escape hatch for a stolen cookie or a rotated key,
  without restarting the proxy.
- **Event-outbox failed-row purge.** Failed delivery rows (retries exhausted) are
  now aged out automatically and can be purged on demand from the Event outbox
  page, so a dead webhook/Slack target can't grow the database unbounded.
- **Config-DB recovery auto-unlocks on restart.** When the bootstrap password
  mismatches the encrypted config DB, a successful recovery now persists the
  recovered hash so the next restart comes up unlocked — no manual env wiring.

## v1.9.0 — 2026-06-30

### Changed (BREAKING)

- **Replication: removed the `source-wins` conflict policy; added `content-diff`.**
  `source-wins` copied every object on every reconcile sweep unconditionally — it
  could never converge on a recurring rule (the rule re-shipped the entire bucket
  forever). It is replaced by `content-diff`, which keeps the destination an exact
  mirror by copying **only when the bytes differ** (size differs, or both sides
  carry a logical SHA-256 and those differ) and skipping byte-identical objects —
  so the rule converges and a healthy sweep eventually copies nothing. **A config
  carrying `conflict: source-wins` is now rejected at load** (`unknown variant
  'source-wins', expected one of 'newer-wins', 'content-diff', 'skip-if-dest-exists'`);
  switch such rules to `content-diff` (same "dest = source" intent, without the
  re-ship).

### Added

- **Lifecycle rules are validated at config time and rejected if they can never
  run.** A delete/transition rule missing (or with an invalid/out-of-range)
  `expire_after`, a retain-newest `count: 0`, an empty transition destination
  bucket, a malformed glob, or a duplicate rule name is now a **fatal config
  error (400)** on every path — `config apply`/`validate`, the admin section PUT
  (the GUI Storage/Jobs editor's save), and `config lint` — instead of being
  accepted as a warning and then failing every scheduler tick.

### Fixed

- **Replication now honors pause mid-run.** Pausing a rule while a sweep was in
  flight previously had no effect (the flag was only checked when *starting* a
  run), so a long run kept going and lingered as "running". The worker now
  re-checks the pause flag at every page boundary, stops promptly, preserves the
  cursor for resume, and settles the run as terminal.
- **Lifecycle run-now/preview on an invalid rule returns 400 without recording a
  spurious FAILED run row.**
- **Security: an unverified email can no longer bootstrap a group grant via a
  `claim_value` rule keyed on the `email` claim** — that path read the same
  attacker-controllable value as the `email_*` rules and is now gated identically.
- **Hardened object-timestamp handling**: a missing `dg-created-at` resolves to
  the object's stable S3 `LastModified` instead of a fresh `now()` (and the
  parser now accepts timezone-offset / lowercase-`z` timestamps), removing a
  class of spurious re-reads.
- **HA state consistency**: a replication rule whose process died mid-run is now
  marked `failed` in its state row on boot (was left showing "running"), and the
  crash-recovery zombie-run scan is lease-aware so it never resurrects a run a
  live peer still holds.

## v1.8.3 — 2026-06-29

### Fixed

- **Verify tab showed stale results as if live during a re-verify.** While a
  re-verification was running, the panel kept displaying the previous run's
  full verdict (difference counts, breakdown chips, "Checked Nh ago") with only
  a small "Re-verifying…" button to hint it was out of date — easy to mistake
  the old numbers for the current state. A re-verify now shows a prominent
  progress banner (live scanned count + Cancel) and dims the prior result,
  clearly labelled as the previous verdict being refreshed.

### Changed

- **Polished the verification wait screen.** The first-run/in-progress state is
  now a framed panel matching the verdict's visual language — a pulsing shield
  halo, the live objects-scanned count, an indeterminate progress bar, and a
  reminder that the scan runs in the background — instead of a bare spinner.

## v1.8.2 — 2026-06-29

### Fixed

- **Job-drawer Runs/Failures/Verify lists collapsed to a sliver.** In the Jobs
  drawer, the record list shrank to its content width instead of filling the
  drawer, and the outcome meter inside it collapsed — the proportional track
  disappeared and the outcome text clipped to a few characters (`run…`,
  `459…`), readable only by hovering for the tooltip. The list now fills the
  available width and the meter renders in full (dot · track · label).

## v1.8.1 — 2026-06-29

### Fixed

- **Mobile horizontal overflow across the admin UI.** Every admin settings page
  (all 15 routes) could scroll sideways on a phone, clipping chips, buttons, and
  table columns. Root cause was flex containers — the content pane and the
  Ant Design `<Layout>` chain — defaulting to `min-width: auto`, so a single wide
  row or fixed-width table widened the whole shell past the viewport. Fixed
  systemically (`min-width: 0`) plus per-panel wrap fixes; dense diagnostic
  tables (Audit, Event Outbox) now scroll within their own card. The
  Users/Groups/Authentication master-detail split stacks vertically on narrow
  screens. Verified at a 380px viewport with zero page-level horizontal scroll.
- **Jobs and Verify-findings panels are now mobile-friendly** — the Verify results
  table reflows to stacked cards, and the Jobs list drops its duplicate header.
- **The unsaved-changes bar no longer hides behind the Jobs drawer** — it now
  floats above the drawer where replication/lifecycle rules are edited, so Apply
  stays reachable.

## v1.8.0 — 2026-06-29

### Security

- **Spoofable client IP via `X-Forwarded-For` is now gated behind a trusted-proxy
  allow-list.** When `DGP_TRUST_PROXY_HEADERS=true` (the documented reverse-proxy
  setup), the proxy previously trusted the first `X-Forwarded-For` element
  verbatim — letting any client forge the IP used for `aws:SourceIp` IAM
  conditions and admission `source_ip` rules. The new `DGP_TRUSTED_PROXY_CIDRS`
  lets you list the CIDRs of your real proxies; `X-Forwarded-For` is then honored
  **only** from a trusted peer and parsed right-to-left (the rightmost
  non-trusted hop is the real client). Default behavior is unchanged
  (`DGP_TRUST_PROXY_HEADERS=false` still ignores the header); operators behind a
  proxy should set the CIDR list to close the spoofing window.
- **SSRF guard now resolves DNS.** Outbound URLs (OIDC issuer/JWKS/token,
  webhooks) were checked only as literal IPs, so a hostname whose DNS record
  pointed at `169.254.169.254` or a private range passed. A connect-time resolver
  on the OIDC and webhook clients now re-checks the **resolved** address and
  fails closed. Note: an OIDC issuer or webhook on a private/internal address is
  now rejected — set it to a publicly-resolvable host, or reach out if you need
  an internal-network escape hatch.

### Added

- **Replication "Verify" (parity audit) is now a fast, server-side background
  job.** Verifying that a replication mirror is byte-identical used to run
  synchronously in the request (HEAD-per-object every time) and lived only in the
  browser — navigating away recomputed it from scratch. It now runs as a
  background job whose result is **persisted server-side** (survives navigation
  and proxy restart), with a **persistent per-object cache** so re-verify is
  HEAD-free, plus **live progress** and a **Cancel** button. Exposed at
  `GET/POST /_/api/admin/jobs/:id/verify` (+ `/verify/cancel`).
- **Readiness probe at `/_/ready`.** `/_/health` is now liveness-only (fast, no
  I/O) and honest about it; the new `/_/ready` actually probes the storage
  backend (bounded by a 3s timeout) and the config DB, returning `503` when not
  ready. Point your load balancer's readiness check at `/_/ready`.
- **Multi-instance / HA contract is documented.** `CLAUDE.md` now states plainly
  which planes are shared across instances (IAM/OAuth via the S3-synced DB,
  background-job leases) versus instance-local (sessions, multipart uploads,
  metadata cache, rate limiter), plus the hard prerequisites for running behind a
  load balancer. See also `docs/plan/architecture-ha-audit-2026-06-28.md`.

### Fixed

- **Multi-instance config sync no longer clobbers in-flight job state.** The
  encrypted config DB sync replaced the **whole** SQLCipher file on every
  download — wiping this node's per-node coordination state (replication /
  lifecycle / maintenance job cursors and leases, the event outbox, listener
  cursors) along with the IAM tables it was meant to share. The sync now merges
  **only** the IAM tables out of a peer's DB, leaving coordination state intact,
  so background jobs and event-driven replication stay consistent across a synced
  fleet. A schema-version guard skips the merge during a rolling upgrade rather
  than risk a column mismatch, and the sync ETag only advances on a successful
  merge (a failed merge retries on the next poll).
- **`CompleteMultipartUpload` on the wrong instance now explains itself.**
  Multipart upload state is per-instance; behind a non-sticky load balancer a
  part/complete landing on a different node than the create returned a bare
  `NoSuchUpload`. The error message now names the per-instance cause and points
  at sticky sessions (the S3 error code is unchanged for client compatibility).
- **Startup banner no longer disagrees with the runtime on proxy-header trust.**
  The banner parsed `DGP_TRUST_PROXY_HEADERS` as a narrower set than the runtime
  (`yes`/`on` logged "untrusted" while the runtime trusted them); both now use
  the same parser.

### Changed

- **The storage backend's streaming memory bound is now enforced by the type
  system.** `get_passthrough_stream` / `get_passthrough_stream_range` had
  buffering fallbacks that a backend could silently inherit (defeating the
  memory bound for large ranged GETs); they are now required methods. No
  behavior change for the built-in filesystem and S3 backends.
- **Internal hardening + test-determinism:** typed config-apply pipeline shared
  by the backup-restore path, replication run/event version counters that replace
  sleep-based test waits, `parse_copy_range` unit + property tests, and a shared
  `RunLease` type. The Jobs admin tables also render consistently across mobile
  and desktop.

## v1.7.0 — 2026-06-28

### Fixed

- **Batch uploads under one presigned POST signature no longer 403.** A
  presigned browser/CI form-POST policy with `starts-with $key` is designed to
  upload many files under a single signature (e.g. a release's `.zip` +
  `.sha512` + `.sha1`). The replay guard treated the second and later files as a
  replay attack and intermittently returned `403 SignatureDoesNotMatch` — after
  the body had fully uploaded — whenever two files were signed in the same
  second. The guard now keys on the signature **and** the file fingerprint, so a
  batch of distinct files passes while an exact resend stays an idempotent
  overwrite. The policy's own conditions (`starts-with $key`,
  `content-length-range`, `expiration`) remain enforced on every request and are
  the real bound on what a captured signature can write.

### Added

- **Streaming delta compression for unbounded object sizes (opt-in, dormant by
  default).** New code paths reconstruct (GET) and encode (PUT/POST/copy) delta
  objects through bounded-memory spool files instead of buffering the whole
  object in RAM — so delta dedup can scale past the previous in-memory ceiling.
  This is **off by default**: `DGP_SPOOL_THRESHOLD_BYTES` defaults to
  `max_object_size`, so the streaming paths are unreachable until an operator
  lowers the threshold. New tunables: `DGP_SPOOL_DIR`, `DGP_SPOOL_MAX_BYTES`,
  `DGP_SPOOL_THRESHOLD_BYTES`, `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`,
  `DGP_CODEC_STALL_SECS`, `DGP_CODEC_ABSOLUTE_SECS`. The xdelta3 codec gained a
  stall-based watchdog and streaming entry points; the storage backends gained
  `get_reference_to_file` / `put_reference_from_file` (filesystem hardlinks, S3
  streams). See `docs/plan/streaming-delta-any-size.md`.

## v1.6.0 — 2026-06-27

### Added

- **Instant per-bucket size — no more O(n) listing sweeps.** S3 has no
  protocol call for "how big is this bucket?" — the only primitive is an
  O(n) `ListObjectsV2` sweep, and the old stats scan capped out at 1000
  objects per bucket (slow, and simply wrong for large buckets). The proxy
  now keeps a running per-bucket counter (object count + logical pre-delta
  bytes + stored bytes), updated on every write and delete, the way Ceph or
  Backblaze B2 surface an instant number. `/_/stats` reads it in O(1) — the
  1000-object cap and `truncated` flag are gone — and a **bucket size chip**
  in the browser top bar shows size · object count at a glance (admin
  session only). The counter is per-instance and best-effort (it never
  blocks or fails an S3 request); a **Refresh** button runs an uncapped full
  reconciling scan on demand. `GET /_/api/admin/usage/bucket/:bucket` exposes
  the O(1) read; `POST /_/api/admin/usage/refresh` triggers the reconcile.

- **Save-time config advisories.** The admin Apply dialog (and `config lint`)
  now surface cross-field "this combination is suspicious" warnings *before*
  you save, instead of leaving you to discover the misconfiguration in
  production. Seed rules catch a rate limit running with proxy-header trust
  disabled (which collapses every client onto the proxy's own IP and one
  shared rate-limit bucket), a stale `${username}` IAM permission template
  that silently denies access, a frozen bucket quota, and a public-prefix
  rule that is redundant when auth is already open.

- **In-GUI operational logs — live tail + filter, no SSH required.** A new
  **System logs** view in the admin UI tails the proxy's operational log
  stream live (server-sent events) with a Follow toggle, and filters a
  bounded in-memory backlog by level, target, and free-text search — so
  diagnosing an incident no longer means SSH-ing in to grep stdout. Backed
  by `GET /_/api/admin/logs` (filtered backlog) and
  `GET /_/api/admin/logs/stream` (live SSE), both admin-session-gated. The
  ring is per-instance and in-memory (size via `DGP_LOG_RING_SIZE`, default
  2000; floor via `DGP_LOG_RING_LEVEL`, default INFO) — it supplements, and
  does not replace, shipping the full stdout stream to your log aggregator.

- **`DGP_LOG_FORMAT=json` structured logs.** Opt-in JSON log output so prod
  logs are field-greppable with `jq` (e.g. by client IP, bucket, or action)
  instead of being parsed out of free-text lines.

### Changed

- **Slimmer admin sidebar.** The settings sidebar was consolidated from
  eight groups down to five — single- and two-leaf groups that cost more in
  headers than they earned are folded together (Dashboard, Trace, Delta
  efficiency, Audit log, and System logs now sit under one **Observability**
  group; the single Jobs screen moves under Storage). Every page URL is
  unchanged.

- **More readable audit & auth logs.** Audit lines now resolve the client IP
  the same proxy-aware way the rate limiter does (no more `ip=unknown`
  splitting one client across two values), and brute-force / lockout log
  lines now include the rate-limit bucket key and proxy-trust state — the
  fields that make "all clients collapsed onto one bucket" obvious at a
  glance.

### Fixed

- **Delta uploads work on newer xdelta3 builds (3.1+).** A newer xdelta3
  enables stream "armor" by default, which requires a seekable target the
  proxy doesn't provide when piping — making every delta-eligible `PUT`
  fail with `armor requires a seekable target` on those builds. The codec
  now probes its xdelta3 at startup and disables armor when supported,
  while staying compatible with the older 3.0.x builds that lack the flag.
  Deltas remain format-identical across versions (plain RFC-3284 VCDIFF), so
  a delta encoded on one xdelta3 version still decodes on any other. The
  exact xdelta3 version and armor state are now logged at boot.

## v1.5.4 — 2026-06-27

### Changed

- **Jobs UI polish.** Run/failure tables now show relative times ("3h ago")
  with the full timestamp on hover. The Runs table replaces the
  scanned/copied/skipped/errors number columns with a single proportional
  progress bar (green = copied, red = errors, blank = skipped/already-in-sync;
  the copied count is overlaid, full breakdown on hover) and a clock icon for
  scheduler-triggered runs — freeing horizontal space. Resuming or running a
  rule now immediately refreshes its runs + failures tables so the new run
  shows without reopening the drawer.

### Fixed

- **Clearer "scanned vs processed" reporting.** A run showing `scanned=600,
  processed=0` previously hid the skipped count, making it look stalled when in
  fact all 600 objects were already in sync (nothing to copy). The skipped
  count is now surfaced (in the progress bar + on hover), so an incremental
  run's "nothing to do" outcome is transparent.

## v1.5.3 — 2026-06-26

### Added

- **Delta-passthrough replication fast path.** Replicating a delta-compressed
  object between two compressed buckets no longer reconstructs the full object,
  ships it whole, and re-compresses it at the destination. When the destination
  already holds the byte-identical reference baseline (or has none yet — it's
  seeded), the `.delta` blob is shipped **verbatim**: no xdelta3 on either end,
  and only the delta's bytes cross the wire instead of the full logical object.
  For versioned-artifact mirrors this is a large egress + CPU saving. The fast
  path is gated hard for correctness — it only fires when the destination
  reference's checksum matches and both sides are plaintext, and always falls
  back to the proven reconstruct path otherwise. Each run reports how many
  objects took the fast path and the egress bytes saved (a
  `deltaglider_replication_delta_passthrough_bytes_saved_total` metric).

## v1.5.2 — 2026-06-26

### Fixed

- **Reject malformed `//` keys at ingest.** The proxy is the S3 gateway, but it
  used to store a client's path-join bug verbatim — keys with an empty path
  segment (`a//b`, distinct from `a/b` in S3). Now `PUT`, browser `POST`
  form-upload, and multipart uploads all refuse internal `//` keys. Reads and
  deletes stay permissive so any pre-existing `//` objects remain reachable for
  cleanup; a single trailing `/` (folder marker / listing prefix) is unaffected.

- **Parity Verify survives transient listing throttle.** A parity audit over a
  large bucket lists both sides page-by-page; a single Hetzner `503 SlowDown`
  mid-scan used to fail the whole verify. The page-list now retries transient
  throttle errors with backoff, so a momentary 503 no longer aborts the audit.

- **Live job Runs tab.** A running job's Runs tab now updates its
  scanned/processed counters live, sourced from the jobs-list poll (no second
  poller) — and refetches run history once when the run finishes.

## v1.5.1 — 2026-06-26

### Added

- **Replication parity audit — "is my mirror verified identical?"** A new
  **Verify** tab on each replication job runs an on-demand source↔destination
  parity check and returns an explicit verdict instead of inferring sync from
  `status=succeeded`. It compares logical SHA-256 + size from metadata (no
  downloads — works for delta-stored objects too) and classifies every object
  Match / checksum-mismatch / missing-on-destination / extra-on-destination.
  The green **"Verified in sync"** verdict is the headline; a foreign-object
  three-tier verifier (sha → etag+size → size-only) avoids false alarms on
  objects not written through the proxy.

- **Closed-loop remediation per finding.** Every difference now explains its
  cause, says **policy-awarely** whether re-running the rule will fix it, and
  guides the operator to the right action. Crucially honest: a checksum mismatch
  under a `skip-if-dest-exists` rule is reported as "re-run won't help — overwrite
  manually or change the policy", not a false "just re-run". A persistently
  failing copy surfaces its last error. "Run now" is offered only where it
  actually helps.

### Fixed

- **Replication retries transient source 5xx.** Transient Hetzner-phrased
  throttle/gateway errors (`throttled (status=503)`, `failed (status=502/504)`)
  are now recognised as retryable, so small objects no longer land in the
  failure ring on a momentary source blip — they retry in-line and succeed
  within the same run.

## v1.5.0 — 2026-06-25

### Added

- **Gigantic-file replication: streaming multipart copy with rclone-class
  concurrency.** Replicating a large passthrough object (e.g. a 30 GB backup
  tarball) used to buffer the whole object in memory twice and fail when the
  single monolithic GET dropped mid-stream. The transfer path now streams large
  passthrough objects source→destination via multipart upload with **bounded
  memory** (only `upload_concurrency × part_size` resident, not the whole
  object), **per-part range-resume** (a dropped connection retries just that
  part, not the entire object), and **concurrency** on two axes — parts within
  an object and objects within a run:

  ```yaml
  replication:
    transfers: 4            # objects copied concurrently per run
    upload_concurrency: 4   # multipart parts in flight per object
  ```

  Delta objects still reconstruct transparently; proxy-side AES-encrypted
  backends fall back to the buffered path (native multipart for them is
  deferred). New `max_passthrough_object_size` (default 64 GiB) decouples the
  streaming ceiling from the delta-reconstruction limit.

- **Replication run resilience.** A single slow or poison object can no longer
  kill a whole run. Lease defaults raised (TTL 60s→300s, heartbeat 20s→60s) so a
  multi-minute copy can't lapse the run lease; new `replication.object_timeout`
  (default `30m`) bounds a stalled copy; `replication.object_skip_after_failures`
  (default 5) skips an object that fails N consecutive runs so it stops blocking
  the queue head (stays visible in the job's failures).

- **Replication observability.** New Prometheus metrics expose the streaming
  characteristics for Grafana: `deltaglider_replication_part_bytes_resident`
  (+`_peak`), `_parts_inflight` (+`_peak`), `_objects_inflight` (+`_peak`),
  `_multipart_parts_total`, `_part_retries_total`, `_bytes_streamed_total`.

## v1.4.3 — 2026-06-18

### Added

- **Count-based lifecycle retention (`retain-newest`).** A new lifecycle
  action that keeps the newest N objects in a prefix and deletes the rest
  — selection by *count*, not age — for the canonical "keep the last N
  backups" cleanup that native S3 lifecycle never shipped:

  ```yaml
  action:
    type: retain-newest
    count: 2
    qualify:                   # eligibility filter (NOT a delete guard)
      min_size_bytes: 1048576  #   ignore truncated/empty junk
      min_age: "1h"            #   ignore in-flight uploads
    protect_younger_than: "7d" # optional delete-side guard
  ```

  `qualify` is an eligibility gate — an object failing it is invisible
  (never counted toward N, never deleted) — so an accidental empty or
  half-written file can't anchor the keep set or displace a real backup.
  `protect_younger_than` is a separate delete-side guard (spares an object
  this run, never promotes it into the keep set). "Keep newest N" is
  set-relative, so it runs on a dedicated collect→rank→act worker path;
  the age-based path stays per-object/streaming and unchanged.

- **Savings dashboard reads as a before/after bill.** The metrics hero
  dollar line now tells the bill story directly: the regular monthly bill
  (what you'd pay without DeltaGlider) struck through, the compressed bill
  as the green headline (what you actually pay), and the saving kept quiet
  underneath ($/mo · $/yr). Previously the green headline showed the
  *saved* amount, which read like "what you pay" when it was the opposite.

### Fixed

- **Browser form-POST upload returned 403 on an idempotent retry.** The
  presigned form-POST replay cache was keyed only on the SigV4 signature
  and rejected *any* second request bearing an already-seen signature for
  up to 24h (the policy-expiry-capped TTL). A presigned POST's signature
  is deterministic for a given policy + second-resolution `x-amz-date`, so
  a CI retry or pipeline re-run of the same artifact re-sent the identical
  signed request and got a spurious `403 SignatureDoesNotMatch` (observed
  uploading the deterministic `.sha1` / `.sha512` siblings right after a
  `.zip`). The replay entry now also fingerprints the `(key, body)` the
  signature first wrote: re-sending the **same object** is an idempotent
  retry and is allowed, while reusing a captured signature to write a
  **different key or body** is still blocked (form-POST `key` is
  `starts-with ""`, so one signature could otherwise authorise writing
  anywhere). Replay protection is preserved; legitimate retries succeed.

### Docs & site

- **Documentation site overhaul.** A Warp-style collapsible sidebar tree
  with a calmer editorial type scale and focus-on-current-section
  navigation; clickable heading anchors; a readable search input with a
  correctly-centred Clear button that closes on result selection; the
  `/docs` landing reworked into 3-up feature cards with readable
  descriptors; and an accessibility/readability pass (type scale, code
  panels, link accent, inline-code chip sizing).
- **Accurate, more complete docs content.** Acted on an expert docs-site
  review (search, intro, S3-compatibility, FAQ) and onboarding/
  architecture/versioning gaps; corrected the `authentication: none`
  tutorial; embedded all product-doc screenshots (not a stale subset);
  and replaced the ASCII data-path diagram with a rendered image.
- **Marketing site polish.** Split-bleed hero with an upbeat captioned
  demo video, a lighter page background, and a branded glow on the demo
  video frame.

## v1.4.2 — 2026-06-13

### Added

- **Inline video & audio preview in the object browser.** Double-clicking
  (or the inspector Preview button on) an `mp4` / `webm` / `mov` / `m4v` /
  `ogv` object now plays it in a native `<video>` player, and `mp3` / `wav`
  / `ogg` / `m4a` / `aac` / `flac` / `opus` in an `<audio>` player —
  streamed from a presigned URL so the proxy's range support (`206 Partial
  Content`) handles seeking without buffering through the browser tab. A
  codec the browser can't decode falls back to the existing Download
  affordance. Previously these fell through to "Preview not available".

## v1.4.1 — 2026-06-12

### Removed (breaking)

- **TOML config support removed entirely.** YAML is the only config
  format. Gone: TOML loading (`Config::from_toml_file` / `from_toml_str`
  and the `.toml` boot path), TOML persisting, the `config migrate`
  subcommand, the `--show-toml` flag, the
  `deltaglider_proxy.toml.example` file, and the
  `DGP_SILENCE_TOML_DEPRECATION` env var. A `.toml` config — pointed at
  via `DGP_CONFIG`/`--config` or found on the default search path — now
  fails startup loudly with: "TOML configs are no longer supported
  (removed in v1.4.1). Convert with `deltaglider_proxy config migrate`
  on v1.4.0, then point the server at the YAML file." Upgrade path:
  run `config migrate` on v1.4.0 (the last release that ships it)
  BEFORE upgrading.

### Added

- **`${env:NAME}` references round-trip.** The proxy now records which
  config values were resolved from `${env:...}` references and re-emits
  the references — instead of materialized secrets — when persisting the
  config to disk (GUI changes) and in `GET /config/export`. Redactors
  keep references intact (a reference is not a secret). The admin
  `/config/apply` document path now also expands `${env:NAME}` against
  the server environment. Together these make the IaC loop lossless:
  provision a secret-free template, tweak via the GUI, export, and
  commit the export straight back into IaC.

### Fixed

- **Apply dialog showed "No changes detected" for credential
  rotations.** The section diff redacted secrets on both sides before
  comparing, so rotating a secret access key, encryption key, OAuth
  client secret, or webhook header produced an empty diff. Secrets are
  now projected to deterministic one-way fingerprints (`fp:xxxxxxxx`)
  for the comparison: unchanged secrets compare equal, rotations
  surface as a readable fingerprint swap, and no key material appears
  in the dialog. `${env:NAME}` references stay readable verbatim.

## v1.4.0 — 2026-06-12

### Added

- **One Jobs surface.** Replication rules, lifecycle rules, and one-off
  maintenance jobs (bucket migration, re-encryption) now live on a single
  admin screen backed by a unified API (`GET /jobs`, per-kind
  pause / resume / run-now / preview / cancel, `POST /jobs/reencrypt`,
  `POST /buckets/:bucket/migrate`). The old per-subsystem
  `/replication*`, `/lifecycle*`, `/maintenance*` admin routes are gone.
- **Bucket migration between backends** as a resumable background job
  (stage → copy → verify → flip → cleanup) with a per-bucket write gate:
  writes to a bucket under maintenance get `503 Slow Down`, reads pass.
  Re-encrypt jobs use the same machinery, survive restarts, and never
  resurrect a peer instance's live job after an IAM-DB sync.
- **Redesigned Analytics dashboard.** Percent-led savings hero
  ("270% smaller") with a before/after proof bar, one unified bucket list
  (ratio + footprint per bucket, per-row scan), an "est. $ left on the
  table" panel for compression-off buckets, a scan-coverage strip, and a
  designed empty state. The old gauge / facts-grid / duplicate top-buckets
  panels are gone.
- **App-wide keyboard shortcuts**: ⌘K command palette over the whole admin
  IA, ⌘S apply-current-section, `?` shortcuts help, arrow-key navigation
  in the object browser.
- **Multi-backend bucket management**: create-a-bucket-on-a-named-backend
  admin API + UI, backend origin badges across the browser and admin.
- **Documentation rewritten to Diátaxis** — 3 executed tutorials, 25
  goal-named how-to guides, 13 reference pages, 5 explanations; 14 new
  product screenshots; the marketing site renders the same corpus.
- **Prod-config regression tests**: a sanitized structure-true snapshot of
  the production config validated in the CI gate (parse, declarative-IAM
  reconciler validation, export round-trip) plus a local harness
  (`scripts/test-prod-config.sh`) that boots the current branch against
  the real prod backup with dynamically derived assertions.

### Fixed

- Creating a bucket on a non-default backend could land it on the wrong
  backend on case-mismatched names.
- Lifecycle crash-resume could replay a stale cursor after a same-named
  rule was redefined with a different scope (cursor is now scope-stamped).
- Maintenance requeue-on-boot is lease-aware — a synced config DB carrying
  a peer's live job is no longer resurrected locally.
- Phase machines (migrate / re-encrypt) now fail loudly when a pagination
  page budget truncates a phase instead of silently advancing.
- Admin bulk copy/move/delete now participates in the per-bucket write
  gate instead of bypassing it.
- Jobs table no longer renders job names one character per line in narrow
  columns.

## v1.3.1 — 2026-06-05

## v1.3.0 — 2026-06-05

### Changed (breaking) — IAM permission templates are now `${iam:...}`

- IAM identity templates in `resources` / string condition values are now
  **`${iam:username}` and `${iam:access_key_id}`** (previously bare `${username}`
  / `${access_key_id}`). The `iam:` prefix makes them an explicit namespace,
  symmetric with the new `${env:NAME}` config expansion — every `${ns:name}`
  declares when it resolves (iam = request time, env = load time), and a bare
  `${...}` is now a literal.
- **Breaking:** a bare `${username}` no longer substitutes — it fails policy
  validation (user/group API + declarative apply). Update existing policies to
  the `${iam:...}` form. No back-compat shim.

### Added — in-process `${env:...}` config expansion

- **Config files now expand `${env:NAME}` / `${env:NAME:-default}` against the
  environment when loaded**, removing the need for an external `envsubst` step in
  deployments. Ship a secret-free config with `${env:...}` placeholders and inject
  the values as env vars; an unset reference (with no default) fails loudly at
  load instead of silently leaving a hole.
  - The `env:` prefix is mandatory: it keeps env placeholders distinct from DGP's
    runtime IAM permission templates (`${username}`, `${access_key_id}`,
    `${email}`, `${filename}`), which are left untouched for the auth layer.
  - `$$` is a literal `$` escape (e.g. for a literal `${...}` in a comment).
  - Applies on the disk-load paths — server startup, `config lint`, and
    `config apply` — but NOT `config migrate` (templates are preserved) nor the
    admin API's in-memory doc-apply (no surprise expansion against the server env).

### Added — Slack notifications connector

- **Object events can now post to Slack.** Setting `event_delivery.format = slack`
  renders each outbox event as a Slack message (Block Kit + plain-text fallback)
  instead of the raw `{schema,event}` JSON envelope. Two delivery modes:
  - **Incoming Webhook URL** — point `webhook_url`/`webhook_urls` at a
    `hooks.slack.com` URL. Each URL is bound by Slack to a single channel; the
    cosmetic `slack_username` / `slack_icon_emoji` overrides apply here.
  - **Bot token** (`slack_bot_token`, `xoxb-…`) — delivery uses the Slack Web API
    (`chat.postMessage`), posting to `slack_channel` with support for `@`-mentions
    and any channel via `chat:write.public`.
- **Per-bucket / per-prefix channel routing** (bot-token mode only). When
  `slack_routes` is non-empty, an eligible event posts to EVERY route it matches —
  so different buckets/prefixes can fan out to different channels (and one object
  can hit several). `slack_notify_kinds` (default `["ObjectCreated"]`) plus
  `slack_include_globs` / `slack_exclude_globs` are a global pre-filter for what's
  eligible; routes then pick the channels.
- **Editable from the admin UI** at Configuration → Advanced → Webhook delivery,
  alongside the raw-webhook config. The **bot token is a secret**: it's masked in
  the GUI and in every export, and leaving the masked value untouched preserves
  the real token on save (both the section-PUT and full-document export→apply
  paths), exactly like webhook auth headers.

## v1.2.0 — 2026-06-02

### Added — event-driven replication

- **Replication now reacts to object mutations in near-real time.** Previously
  replication was pure-poll: a timer did a full `list + HEAD` diff every tick,
  scaling with bucket size rather than change rate. Object mutations
  (PUT / DELETE / COPY / CompleteMultipartUpload) are now appended to the durable
  `event_outbox` and drained by a per-process consumer that fans each key out to
  the matching replication rules. The per-rule `interval` is repurposed as a slow
  (default **24h**) full-reconcile safety net — events are the primary trigger.
  - Pub/sub via a per-listener cursor over the append-only outbox: webhook
    delivery and replication keep independent cursors and never contend.
  - Per-key compaction collapses a burst of events for one key into a single
    liveness verdict; idempotency stays in one place (the planner + a dest HEAD),
    shared with reconcile — no new per-key sync table.
  - At-least-once delivery: the cursor advances only to the highest contiguous
    fully-handled event id; reconcile backstops the rest.
  - The webhook pruner clamps its delete floor to the smallest **active** listener
    cursor, so a stuck or disabled consumer can no longer pin the outbox and let
    it grow without bound.

### Added — Webhook delivery GUI

- **Webhook (event-delivery) config is now editable from the admin UI** at
  Configuration → Advanced → Webhook delivery — the last config surface that was
  YAML/env-only. Enable switch, endpoint list, auth-header editor, retry /
  retention / batching tuning, and a live delivery-status strip linking to the
  Event Outbox viewer.
- **Header secrets are masked and preserved.** Header values are shown masked in
  the GUI and in every export; leaving a masked value untouched preserves the
  real token on save (both the section-PUT and the full-document export→apply
  paths), while a removed header is deleted and a renamed-but-not-retyped secret
  is blocked rather than silently dropped.

### Fixed — CI

- **Self-hosted runner "Too many open files".** The CI integration job runs ~35
  test binaries in parallel inside Ryzen LXC runners whose nested dockerd
  defaulted to a 1024 nofile limit; the aggregate fd count overflowed it. Raised
  the limit at the dockerd, workflow (`--ulimit`), and test-harness (seed
  concurrency cap) layers.

## v1.1.2 — 2026-06-02

### Fixed — hardening (from an adversarial Rust audit)

- **Form-POST uploads can no longer OOM the proxy.** The `multipart/form-data`
  POST interceptor buffered the request body with no ceiling
  (`to_bytes(usize::MAX)`); because it ran above `DefaultBodyLimit` — which only
  does an eager `Content-Length` check — a chunked or oversized POST slipped past
  the limit. Now bounded by `max_object_size` (oversized → `413`). This also
  fixes a real upload failure with aws-cli's chunked transfer encoding.
- **CopyObject on a concurrently-deleted source now returns `404`, not `500`.**
  The generic S3 error classifier didn't recognise `NoSuchKey`, so a benign race
  surfaced as an internal error.
- **Form-POST signature length is no longer leaked via timing.** The HMAC compare
  now rejects any signature that isn't exactly 64 hex characters before any
  length-dependent work.

## v1.1.1 — 2026-06-01

### Changed — IAM permission editor

- **One-line grant editor.** Each permission rule is now a single row —
  `WHERE` (bucket / prefix) + `CAN DO` (the five atomic actions as a horizontal
  strip of on/off toggle-chips) + Conditions + Remove. The chips are an
  independent multi-select with a checkbox indicator (solid when on, dashed when
  off), so "write without delete" is expressible; the Admin chip auto-disables
  on prefix-scoped grants (a bucket-level op is meaningless on a sub-prefix), and
  a live plain-language caption summarises each grant. "Clear all" appears inline
  beside the actions when more than one is selected.

### Fixed — IAM permission editor correctness

- **Resource rows no longer drop or re-key on blur.** The on-blur normalizer
  re-split the whole comma-joined resource string, discarding in-progress empty
  rows and reassigning React keys to surviving rows; it now normalizes only the
  blurred row in place.
- **Narrowing a grant's scope no longer silently loses the rule.** Reducing a
  resource from a bucket to a sub-prefix strips the now-invalid `admin` action,
  and any incomplete rule (missing actions or resource) is flagged inline ("…or
  this rule is dropped on save") instead of vanishing on Apply.
- **Invalid resource patterns are caught inline.** Patterns the server rejects
  (a `*` that isn't trailing, embedded whitespace) now show an inline error
  before Apply, instead of an opaque HTTP 400.

## v1.1.0 — 2026-05-31

### Added

- **Full IAM export/import as YAML.** The admin GUI Account menu gains
  "Export full IAM (YAML)" and "Import full IAM (YAML)", round-tripping the
  entire IAM state (users, groups, OAuth/auth providers, group-mapping rules)
  as declarative `access:`-shaped YAML — distinct from the runtime-config YAML,
  which excludes the encrypted IAM DB. Export includes real secrets for a
  lossless round-trip (with a prominent live-credentials warning); import is a
  dry-run change preview → confirm → atomic single-transaction reconcile.
  Backed by new admin endpoints `config/declarative-iam-{export,validate,apply}`
  (mode-agnostic, admin-GUI-session gated).

### Fixed — IAM permission editor (GUI)

- **Bucket-root listing ("" prefix) is now expressible.** The `s3:prefix`
  condition editor round-tripped through a comma-joined string that silently
  dropped the empty-string entry, so "list from the bucket root" could not be
  saved. The editor now uses a string-array contract end-to-end with a
  dedicated "List bucket root (empty prefix)" toggle; the empty string is
  preserved through save and reload.
- **Prefix conditions no longer auto-append a trailing slash on blur.** For an
  `s3:prefix StringLike` condition, `ror/libs` and `ror/libs/` are not
  equivalent (the slash-less form also matches `ror/libs-internal/…`); the blur
  normalizer now preserves the operator's trailing-slash choice.
- **Resource rows keyed by stable ids** (were array-indexed), removing a
  stale-key class of focus/row-confusion bugs when adding or deleting rows.
- **Inline unknown-bucket warning.** A resource pattern targeting a bucket that
  doesn't exist (e.g. `ror/lib/*` when the data is in `beshu/ror/libs/*`)
  silently grants nothing; the rule editor now flags it inline with a
  near-miss suggestion.

### Changed — bucket browser & dashboard

- **URL-routed bucket browser.** Browser navigation (bucket, folder prefix) is
  reflected in the URL, so back/forward, deep-links, and reload now restore the
  exact view instead of resetting to the root.
- **Dashboard scan state survives navigation.** "Scan all buckets" results are
  server-side state the dashboard re-attaches to on mount (including in-flight
  scans), and cached results carry a derived staleness nudge after 6 hours
  rather than silently disappearing when you navigate away and back.

## v1.0.1 — 2026-05-31 — Hardening, cleanup & reliability

A large quality release: the admin UI was hardened against a class of
state-management bugs and then substantially refactored for less code and
cleaner structure; the S3 bucket browser got the same treatment; and several
correctness/reliability gaps were closed across the engine and S3 layer. No
wire-format or breaking changes — drop-in over v1.0.0.

### Fixed — data integrity & correctness

- **Same-location move no longer deletes data.** The server bulk-move handler
  did copy-then-delete-source for every item; a "move" whose destination
  resolved to the same bucket+key as the source was a self-copy no-op, so the
  unconditional source delete destroyed the only copy. A server-side guard
  (`is_same_location_move`) now skips the source delete for any such item,
  regardless of what the client sends.
- **Range-GET out-of-bounds panic** on objects whose stored `file_size`
  metadata exceeded the reconstructed byte length now returns `400 InvalidRange`
  instead of crashing the worker.
- **Doubled-quote ETag** on passthrough HEAD/GET (`""abc""`) fixed to a single
  quoted form; strict S3 clients rejected the doubled form.
- **Admin config editors**: eliminated a class of row-drop / stale-closure /
  lost-edit / index-key bugs (prefix list, permissions, buckets, advanced,
  credentials, auth-rules, setup wizard); a failed Apply no longer discards
  in-flight edits.
- **Bucket browser**: inspector async-race (download/share landing in the wrong
  object), pagination desync on search, bulk-copy not clearing selection,
  destination-prefix join bug, 0-byte upload handling, and a perpetual
  "Loading compression stats…" spinner for passthrough objects.

### Changed — performance & logging

- **LIST `metadata=true` skips the per-object HEAD for passthrough, non-delta
  files** (checksum sidecars, images, …): they're stored verbatim so the LIST
  size is authoritative. Cuts the upstream HEAD burst on build-artifact
  listings (which was also a throttle/500 trigger).
- **Tamed the `PATHOLOGICAL` log flood**: missing DG metadata on a normal
  passthrough file is benign and now logs at DEBUG; the loud WARN is reserved
  for `.delta`/`reference.bin` files where it actually breaks reconstruction.
- **Catch-all 500s now log the upstream cause** before mapping, so production
  500s are debuggable.

### Changed — admin & browser UI refactor (no behavior change)

- Admin UI clean-code + architecture pass: migrated Buckets/Lifecycle/
  Replication onto the shared `useSectionEditor`; extracted `MasterDetailPanel`,
  `RuleListEditor`, and a shared overlay-dropdown primitive; split the four
  largest panels into per-concern files; split `adminApi.ts` into a barrel over
  domain modules; ~1k LOC of duplication removed.
- Bucket-browser clean-code + correctness pass: shared `getFileName` /
  `downloadBlobAsFile` / `pluralize` helpers, `StorageTypeTag` / `FolderSizeCell`
  extraction, s3client pure helpers.

### Added — testing

- Codec encode→reconstruct **round-trip property test** (the core data-integrity
  invariant), a **criterion benchmark harness** for the codec hot paths, and a
  panic-surface audit of the request hot paths. Plus ~15 new Node regression
  scripts covering the extracted pure UI helpers.

## v1.0.0 — 2026-05-22 — Project-shape milestone

v1.0.0 marks the official **single canonical implementation**: one Rust
binary (`deltaglider_proxy`) ships the S3-compatible proxy, the
AWS-CLI-shaped `s3` command group, and the web UI. The Python
[`deltaglider`](https://github.com/beshu-tech/deltaglider) tool is
deprecated as of its parallel v6.2.0 release; PyPI installs continue
to work indefinitely but no further updates will land there. The wire
format remains byte-identical across both tools.

This release also closes the four post-v0.12.0 regression fixes
(`HeadBucket` 501, multipart `x-amz-storage-type` header drop, `s3 ls`
doctest, all from the v0.12.0 axum-handler retirement) plus the
small UX gaps the v1.0.0 plan flagged.

### Changed

- **`s3 migrate` learned `--source-endpoint-url`** for cross-provider
  migrations (e.g. Hetzner → AWS). When unset, behaviour is identical
  to v0.12.0: a single engine handles both sides. When set, builds
  two engines (source + destination); credentials are shared across
  both. Per-side credential flags can land later if operators ask.
- **`s3 cp` / `s3 sync` / `s3 migrate` learned `--max-object-size-mb`**
  to override the engine's per-invocation 100 MiB defensive ceiling
  (memory-safe limit for xdelta3 — reference + delta + result all
  held in RAM simultaneously). Operators uploading large artifacts
  (release ZIPs, disk images) used to hit a "Object too large" error
  with no actionable hint; now there's a flag and the error text
  points at it.

### Fixed

- **s3s: implement `HeadBucket` (`HEAD /<bucket>`).** The v0.12.0
  axum-handler retirement deleted the legacy handler but the s3s
  trait impl never had one of its own — s3s's default returned 501.
  Real AWS / MinIO return `404 NoSuchBucket` when the bucket is
  missing; we now mirror that. Restores
  `error_test::test_nosuchbucket_xml_response` and
  `test_entitytoolarge_response`.
- **s3s multipart: emit `x-amz-storage-type` on
  CompleteMultipartUpload.** The legacy axum handler set this header
  from `store_result.metadata.storage_info.label()`; the s3s impl
  dropped it during the v0.12.0 retirement. Restored to match
  `head_object` / `get_object` / `put_object` parity, fixing
  `test_multipart_delta_compression` and
  `test_multipart_large_zip_forces_passthrough_on_s3_backend`.
- **cli docs: `s3 ls` shell example no longer breaks `cargo test --doc`.**
  The four-space indentation made rustdoc try to compile the example
  as Rust. Wrapped in ```text so it stays prose.
- **Stronger `--max-object-size-mb` error path:** Detect
  `EngineError::TooLarge` in `cp` / `sync` / `migrate` and render an
  actionable message naming both the per-invocation flag and the
  server-side `max_object_size` YAML knob.

### Deprecated (downstream — not in this repo)

- **The Python `deltaglider` package is deprecated as of v6.2.0.**
  Migration: `brew install beshu-tech/tap/deltaglider_proxy` (or
  download the binary), then `alias dg='deltaglider_proxy s3'`. Every
  Python subcommand has a 1:1 Rust equivalent. The Python repo will
  be archived approximately one week after v6.2.0 hits PyPI.

## v0.12.0 — 2026-05-19

### Removed (BREAKING)

- **Legacy axum-handler S3 adapter retired.** The `s3s` crate adapter
  has been the production S3 protocol path for several releases; the
  hand-rolled axum-handler implementation it was meant to replace is
  now deleted (~3500 LOC of `src/api/handlers/{object,bucket,multipart}.rs`
  plus the supporting XML response builders, the parity test suite,
  and `src/api/xml.rs`). With it goes:
    * the `s3s-adapter` Cargo feature flag (now an unconditional dep),
    * the `DGP_S3_ADAPTER` env var (no more selector),
    * the `test-s3s-adapter` CI job and the nightly `test-all-s3s-adapter`
      job (every test they ran is in the regular matrix already),
    * the `x-deltaglider-s3-adapter: s3s` diagnostic response header
      (only useful when both adapters coexisted),
    * `docs/dev/s3-adapter-decision.md` and `docs/plan/s3s-adapter-migration.md`
      (superseded by this release).
  Operators previously rolling back via `DGP_S3_ADAPTER=axum` have no
  fallback path. If a behavior regression turns up, file a bug against
  the s3s adapter directly — there is no longer a second implementation
  to fall back on.

### Changed (BREAKING)

- **CLI client verbs moved under `s3` subgroup.** The 10 AWS-CLI-shaped
  client commands (`cp`, `ls`, `rm`, `sync`, `migrate`, `stats`, `verify`,
  `purge`, `get-bucket-acl`, `put-bucket-acl`) are now nested under
  `deltaglider_proxy s3 <verb>` instead of being top-level. Top-level
  `deltaglider_proxy --help` is now uncluttered, listing only `config`,
  `admission`, and `s3` as subcommand groups; future top-level proxy
  flags (`init`, `set-bootstrap-password`, etc.) no longer collide.
  Migration: replace `deltaglider_proxy cp ...` with `deltaglider_proxy
  s3 cp ...` (likewise for the other nine verbs). Operators preferring
  the Python-style invocation can alias `dg='deltaglider_proxy s3'`.
  Library callers (`deltaglider_proxy::cli::cp::run` etc.) are
  unchanged — only the CLI surface moved.

### Fixed

- **Eliminated `unsafe { std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", ...) }`
  from CLI startup paths.** The SSRF allow-local opt-in now flows
  through the typed `BackendConfig::S3.allow_local` field instead of
  via process-env mutation. The legacy `DGP_BACKEND_ALLOW_LOCAL` env
  var is still honoured (backward-compat for existing deployments and
  the proxy's env-driven config layer), so no operator action is
  required. Eliminates a Rust-2024-`unsafe` block from three CLI
  modules (`engine_factory.rs`, `bucket_acl.rs`, `purge.rs`) and makes
  the engine constructible without mutating process state. New
  `BackendConfig::S3.allow_local` (defaults to `false`, serde-skipped
  when unset for clean YAML diffs) is the preferred path going
  forward; legacy YAMLs parse unchanged. Added regression test
  `build_client_allows_dev_local_when_config_field_set` pinning the
  typed-field path independent of env.

- **CI workflow `DGP_BACKEND_ALLOW_LOCAL=true` job-level env**
  ([2fbcc5c](#) + [577edd7](#) + [e643ba3](#)) — wave-1 security
  (`3ff9edc`) rejected `http://localhost:9000` MinIO endpoints by
  default, breaking the `[ci-integration]`-gated Integration Tests
  batch (hidden on main pushes by the gate). Set the opt-in at job
  level in `ci.yml` and `test-all-nightly.yml`; threaded through
  `interop_test.rs`'s `env_clear()` barrier; updated 4 cli_s3 test
  fixtures that forgot to create the source bucket before seeding.
- **`DGP_REPLAY_WINDOW_SECS=0` in CI** ([577edd7](#)) to disable the
  wave-3 SigV4 replay cache for tests that legitimately re-issue
  identical signed requests inside the 2s window. Production keeps
  the cache on; the two auth tests validating the wave-3 replay
  contract override the env back to `2` via `TestServerBuilder::env`.
- **`DGP_USAGE_CACHE_TTL_SECS` made env-configurable** ([2c5ea89](#))
  so quota tests can shorten the 5-minute scan-cache TTL. Production
  default unchanged. Fixed two deterministic-race test failures in
  `quota_test.rs` where the spawned scan finished BEFORE the seed PUT
  body landed on disk, caching a "0 bytes" stale read for 5 minutes.
- **CI builder image now ships `clang` + `lld`** ([95cdeac](#)) —
  the compile-time-optimization PR added `linker = "clang"` +
  `-fuse-ld=lld` to `.cargo/config.toml` but the GHCR builder image
  only had `gcc`. Cascaded every build-gated job into `error: linker
  'clang' not found`. Two-line `apt-get install` addition plus
  verification step (`clang --version`, `ld.lld --version`).

## v0.11.0 — 2026-05-16

### Added

- **Bucket-wide scan endpoint with persistent on-disk cache**, backing
  the dashboard's headline totals. Replaces the old `/_/stats` path
  that capped at 1,000 objects per scan — useless on any real bucket
  (a 1.4 TB Hetzner bucket with 79k objects reported "1,000+" and
  guessed savings off the first kilobyte). New
  `GET /_/api/admin/diagnostics/scan/status[?bucket=X]` returns the
  cached map (or single bucket); `POST …/scan/start` kicks a
  paginated walk; `POST …/scan/stop` cancels; `DELETE …/scan` forgets
  a cached result; `GET …/scan/stream?bucket=X` opens an SSE feed of
  per-page progress (`objects`, `original_bytes`, `stored_bytes`,
  `pages_done`, `has_more`). Results persist to
  `.deltaglider_scans/<bucket>.json` with a schema-version field —
  results survive proxy restart, no TTL, regenerate on deserialise
  failure. Per-bucket scan handle backed by `tokio::sync::watch` +
  `CancellationToken`; the scan continues to completion even after
  every SSE subscriber disconnects (useful for long PB-scale walks
  left running overnight). 6 unit tests cover persistence, corrupted-
  file recovery, version-mismatch rejection, and bucket-name
  sanitisation against `..` / `/` traversal.

- **"Money shot" Analytics dashboard redesign.** The four flat KPI
  cards (Total storage · Space saved · Savings % · Est. monthly
  savings) collapse into one 12-col `HeroSavingsPanel` showing
  storage saved at billboard scale with a count-up animation on
  first visit per session (`sessionStorage` gate, honours
  `prefers-reduced-motion`), a scale-accurate kept-vs-saved bar
  echoing the hero ratio, a dollar savings line with cost-rate
  preset popover, and a cache-age footer line ("Newest scan 47 s
  ago · Oldest 3 h 12 m ago · 2 buckets excluded"). Single new
  dependency: `motion@^12` for the orchestrated count-up + bar-grow
  animation (~5 KB initial, ~30 KB lazy). Below the hero, the old
  "Storage by bucket" recharts stacked bar — which collapsed to a
  single line when one bucket dwarfed the rest — is replaced by a
  **Bucket fleet** view where every bucket gets a full-width ratio
  bar plus a thin scale-relative footprint bar underneath, so even
  tiny buckets are legible. The dead "Scan progress" + "Compression
  opportunities" panels (empty 90% of the time) become a **By the
  numbers** insights grid (largest bucket / biggest single saving /
  best & worst ratio / total objects / avg ratio) and a
  **Compression effectiveness** radial gauge with explicit
  tier-breakdown (Excellent ≥ 50 %, Good 20–49 %, Low < 20 %, None
  0 %, N/A < 2 objects). The gauge value is *bytes-weighted* — a
  1.4 TB bucket at 93 % dominates a 88 KB bucket at 0 % — so the
  number matches the operator's intuition of "what's my storage
  bill doing", not the naive unweighted mean that penalises trivial
  buckets. Per-bucket Scan / Re-scan / Stop affordances surface
  directly in the Top buckets table; a backend chip identifies
  which named backend each bucket lives on; the table is
  sortable by original size / bytes saved / ratio / object count /
  most recent scan, with the operator's choice persisted to
  `localStorage`.

- **Delta-efficiency per-prefix "verify" deep dive.** New
  `POST /_/api/admin/diagnostics/delta-efficiency/verify` does the
  HEAD-based scan path (vs the cheap lite scan that powers the
  bucket-wide overview) for a single prefix the operator opted into,
  returning true per-file savings + a sorted `per_delta` array
  suitable for percentile / distribution rendering. Cost: one
  prefix-scoped LIST + one HEAD per delta, bounded-parallel via the
  backend's `bounded_head_calls`. Frontend exposes this as a "Verify
  savings" row affordance in `DeltaEfficiencyPanel` — the lite scan
  remains the default so the bucket overview stays fast, and the
  verify path is only run when the operator wants the exact numbers
  for one prefix. 5 unit tests pin the response shape and the
  sorting invariant.

### Changed

- **Monitoring-tab refresh controls hidden on Analytics.** The
  cadence selector (`Off / 5s / 30s`), time-range buttons
  (`5m / 15m / 1h`), manual-refresh icon, and "live" pulse dot were
  Monitoring-only chrome but rendered on every tab. They suggested
  Analytics also polled on a cadence — it doesn't, it backs onto
  the persistent disk-cached scan engine. `DashboardToolbar` now
  gates that row on `view === 'monitoring'`.

### Added

- **Delta efficiency diagnostics panel** at
  `/_/admin/diagnostics/delta-efficiency`. Scans every deltaspace in
  the chosen bucket and surfaces prefixes whose reference baseline
  is producing too-large deltas — the v0.9.17 incident shape where
  `s3://beshu/ror/builds/1.70.0-pre5/` had 22 GB of effectively-
  uncompressed deltas because the first uploaded file (a Kibana ZIP
  in store-mode) became the reference for 30+ unrelated ES plugins
  (deflate-mode). Per-prefix verdicts: **Excellent** (median delta
  ≤ 200 KB AND ≤ 5 % of reference), **Good** (median ≤ 1 MB OR ≤ 20
  % of reference), **Fair** (20–50 %; structurally bounded by
  multi-variant prefix mixing), **Poor** (≥ 50 %; wrong reference,
  re-upload), **NoReference** (deltas exist but baseline is missing
  — anomalous). Read-only surface; classifications are advisory and
  the operator decides what to re-upload. Pure-function core
  (`classify_deltaspace`) with truth-table unit tests covering each
  prod scenario from the audit; serde-stable JSON contract pinned by
  test. Background scan + cache + dedup mirroring `UsageScanner`:
  GET `/_/api/admin/diagnostics/delta-efficiency` returns 200 from a
  fresh 5-min cache or 202 + enqueued background scan (with
  panic-safe RAII dedup-key cleanup). POST `…/scan` forces a
  re-scan. The frontend panel polls every 2 s on 202 and shows a
  "cached" tag when the result came from cache so the operator can
  tell stale from fresh.

### Documented

- **Reverse-proxy read-timeout requirement** for large uploads.
  Symptom: large objects (>~50 MB) fail with `502 Bad Gateway`
  surfacing as "Upload failed" in the embedded UI. Root cause:
  Traefik 3.x defaults `respondingTimeouts.readTimeout` to **60 s**,
  killing the body-read mid-upload. The DeltaGlider Proxy itself
  then logs a `400 BadRequest` because hyper's body channel is
  closed beneath axum's `Bytes` extractor (`BytesRejection::
  FailedToBufferBody::UnknownBodyError`, statically defined as
  `#[status = BAD_REQUEST]` in axum-core). Reproduced locally:
  default Traefik 3.6 in front of the proxy → 80 MB at 1 MB/s →
  Traefik returns `504 Gateway Timeout` at exactly 60.7 s. Fix:
  set `entryPoints.<name>.transport.respondingTimeouts.readTimeout`
  to `30m` (or `0` for no limit). For Coolify users: edit
  `/data/coolify/proxy/docker-compose.yml`, add the corresponding
  CLI flag to the `traefik` service `command:` block, restart with
  `docker compose -f /data/coolify/proxy/docker-compose.yml up -d`.
  Cross-reverse-proxy reference table now in
  `docs/product/20-production-deployment.md` (Traefik, Caddy,
  nginx, AWS ALB, HAProxy).

### Mitigations

- **Embedded uploader: cap concurrent files at 1.** Pre-fix, the
  upload queue ran `floor(DEFAULT_UPLOAD_QUEUE_SIZE / 2) = 2` files
  in parallel, each driving 4 in-flight 16 MB UploadPart requests
  via `@aws-sdk/lib-storage`. Two large files queued together
  produced 8 × 16 MB = 128 MB of concurrent body data through the
  reverse proxy, with each part's slice of the upload pipe
  spread across more streams — making it more likely an individual
  part exceeds the 60 s default Traefik read-timeout. Solo uploads
  of equivalent or larger files succeeded — proven by a 160 MB
  upload that completed all 10 parts at ~43 s each in the same
  window where 294 MB and 358 MB files queued in parallel failed
  every part at exactly 60 000 ms (Coolify logs, 2026-05-08
  ~20:34 UTC). Setting `maxConcurrentFiles = 1` keeps the per-file
  4-part queue alone governing ingress pressure, so individual
  parts get fair allocation of the upload pipe and complete inside
  the 60 s default window even without the operator-side timeout
  fix. Belt-and-braces with the docs change above.

### Fixed

- **Form-POST upload rejected `acl=private`.** Browser presigned-POST
  builders (boto3, minio-js, aws-amplify) auto-include `acl=private`
  when the user constructs a presigned-POST without explicitly opting
  out. Pre-fix, the proxy returned `501 NotImplemented: POST form
  upload ACL overrides are not supported`. Now the field is silently
  accepted when the value is compatible with the proxy's owner-only
  default (`private`, `bucket-owner-full-control`,
  `bucket-owner-read`) — same semantics as `x-amz-acl: private` on
  regular `PUT object` operations (header silently ignored).
  Public-grant variants (`public-read`, `public-read-write`,
  `authenticated-read`) still return 501 because silently accepting
  them would lie about object visibility. Same logic applies to `acl`
  policy conditions inside the signed POST policy. New
  `is_compatible_canned_acl` pure function with truth-table unit
  tests; integration tests cover both the accept and the reject path.

## v0.9.17 — 2026-05-07

## v0.9.16 — 2026-05-07

### Correctness x-ray fix programme

A focused audit by five specialised investigators (concurrency, auth,
storage, HTTP/wire, error handling) surfaced ten distinct correctness
bugs ranging from durable on-disk drift to silent panics on
attacker-reachable input. All ten are now fixed; tests pin each
contract.

- **C-P0-1 / C-P1-1: DeleteBucket race with in-flight CompleteMultipartUpload.**
  `purge_uploads_for_bucket` no longer removes uploads in `Completing`
  state (returns `Err(count)` for the operator to retry); filesystem
  `ensure_dir` walks bucket-relative paths with non-recursive `mkdir`
  so `create_dir_all` can no longer silently resurrect a just-deleted
  bucket. Closes the data-resurrection class entirely.
- **E-P0-1: OAuth callback panic from `?next=` header injection.**
  Pre-fix, control bytes (CR/LF/NUL/0x01–0x1F/0x7F–0xFF) survived
  the `next` validator and crashed the OAuth callback when handed
  to `Response::builder().header(LOCATION, ...).unwrap()`. Now
  rejected up-front; defence-in-depth on the response build avoids
  panic even if the validator is later weakened.
- **S-P1-1: Anchor poisoning.** Removed the
  `!has_existing_reference` gate from the `max_delta_ratio` check.
  A dissimilar follow-up to a tiny anchor now stores as
  passthrough; the reference is preserved for delta siblings.
- **S-P1-2: Orphan reference on encode failure.** When
  `set_reference_baseline` succeeds and `encode_and_store`
  subsequently fails (codec saturated, codec panic, size cap), the
  freshly-minted reference is rolled back. Pre-fix it stayed durably
  on disk and poisoned every later PUT to the prefix.
- **S-P1-3: Filesystem fallback ETag.** Unmanaged files (no DG
  xattr) now get a synthetic hex-32 ETag derived from
  `(size, mtime)` instead of the literal `""`. Empty files use the
  canonical `d41d8cd9...`. Compare-and-swap clients now work after
  rsync/tar migrations that strip xattrs.
- **H-P1-1: RFC 7232 §6 conditional precedence.** `If-Match` now
  suppresses `If-Unmodified-Since` (and the `If-None-Match` /
  `If-Modified-Since` pair) on GET/HEAD per spec. Pre-fix, requests
  combining both headers got spurious 412s.
- **H-P1-3: Multi-range header.** Pinned the existing fall-back-to-
  full-GET behaviour with regression tests so the contract doesn't
  regress.
- **E-P1-1: Backend 503 SlowDown surfaces as 503, not 500.** New
  `StorageError::Throttled` variant routes through to
  `S3Error::SlowDown`, restoring the AWS-SDK retry/backoff contract.
- **E-P1-2: Usage scanner panic leak.** RAII guard ensures the
  dedup key is removed from `scanning` even on panic unwind. Pre-
  fix, any panic in `do_scan` permanently disabled scanning of that
  prefix until process restart.

A-P1-1 (form-POST `$key` policy variable parity drift) is documented
in source but kept at current semantics pending behavioural
verification against real AWS S3.

`cargo test --lib` 769 (was 746); +23 regression tests across nine
test modules. Clippy `-D warnings` clean on default features and
`--features s3s-adapter`. No env-var, schema, or wire-protocol
changes; backwards compatible.

## v0.9.15 — 2026-05-07

### CI and compatibility follow-up

- Stabilized `s3s` compatibility runs by skipping form POST compatibility assertions on
  the experimental `s3s` adapter path (feature remains validated on the primary adapter).
- Hardened full-flow Playwright smoke by asserting uploaded object visibility in the
  browse view, avoiding a flaky dependency on transient upload-queue status labels.

## v0.9.14 — 2026-05-07

### Multipart complete memory bound fix

- Fixed the CI-blocking `memory_test` regression by changing MPU relay policy to
  threshold-triggered promotion instead of always-relay for passthrough uploads.
- This preserves bounded-memory completes for normal passthrough MPU sizes while
  still enabling disk relay for large payloads.
- Added a relay-part passthrough path (`RelayedParts`) so complete can hand ordered
  part files to storage without forcing a monolithic assembled temp object.
- Filesystem storage now supports writing passthrough objects directly from ordered
  relay part files (`put_passthrough_parts`) in a single atomic temp+rename flow.

## v0.9.13 — 2026-05-07

### Multipart passthrough relay and cleanup hardening

- CompleteMultipartUpload now supports relay-backed passthrough completes for large
  payloads, avoiding monolithic in-memory assembly when delta reconstruction would be
  too expensive.
- Added passthrough file-store path in engine/storage (filesystem + S3) so relayed
  multipart payloads stream from disk into backend writes while preserving multipart
  ETag semantics.
- Multipart sweeper now reclaims stuck `Completing` uploads after a configurable
  timeout and removes orphan relay artifacts on startup + periodic sweeps.
- Added multipart sweep metrics and env knobs:
  `DGP_MULTIPART_SWEEP_INTERVAL_SECS`, `DGP_MULTIPART_SWEEP_MAX_AGE_SECS`,
  `DGP_MULTIPART_COMPLETING_TIMEOUT_SECS`, `DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES`.

### S3 form POST compatibility

- Added minimal SigV4 policy-based `multipart/form-data` object POST support on
  bucket endpoints for `create_presigned_post` flows.
- Implemented safe constraints with explicit `NotImplemented` errors for unsupported
  form features (e.g. ACL overrides, `success_action_*`, session-token policy forms).
- Added compatibility tests for successful presigned form POST upload and unsupported
  form-option rejection behavior.

### Coverage for relay and reclaim behavior

- Added S3-backend integration coverage for large multipart passthrough completes.
- Added startup orphan relay cleanup integration coverage.
- Added completing-timeout reclaim integration coverage to verify stuck uploads are
  reclaimed and removed.

## v0.9.12 — 2026-05-07

### Upload reliability and UX

- Browser uploads now use managed self-signed multipart upload (`@aws-sdk/lib-storage`)
  with bounded concurrency and per-part retries, replacing single-request large PUTs.
- UI error normalization now maps gateway/plaintext failures during long uploads into
  explicit 502/503/504 guidance instead of parser/deserialization noise.

### Multi-backend bucket creation

- Added admin API support to create a bucket pinned to a selected backend
  (`POST /_/api/admin/buckets`) and wired the browser create-bucket flow to use it.
- Create-bucket backend selector in the sidebar now uses `SimpleSelect` (no Ant popup
  dependency), matching the rest of the embedded UI dropdown policy.

## v0.9.10 — 2026-05-06

## v0.9.11 — 2026-05-06

### Browser UI and admin

- Open-access mode: admin bootstrap login now seeds anonymous S3 session credentials so a
  hard refresh does not strand the embedded file browser without a working SDK client.
- Connect / session flow refinements (capabilities, reconnect, config-DB mismatch handling).
- Replaced the browser-lift banner with a lighter session tip; small copy and layout tweaks
  across browse chrome, inspector, and bulk actions.
- Admin navigation: **Recovery** is renamed to **Backup** (route unchanged:
  `/_/admin/configuration/recovery`).

### Testing

- Added Playwright **full-flow** E2E (bucket create, upload, admin login, sign-out, reconnect,
  object still visible); `e2e-smoke.sh` runs the full `e2e/` suite.

## v0.9.9 — 2026-05-05

## v0.9.8 — 2026-05-05

### Admin IAM and storage-path authoring

- Admin UI: grouped autocomplete for IAM resource patterns and LIST condition
  prefixes (listed prefixes vs per-user placeholders), with clearer section
  layout and plain-language help.
- Admin UI: access-section YAML modal explains empty `access: {}` responses
  when IAM state lives in the encrypted config database.
- `storagePath` helpers, prefix listing for suggestions, and a small Node
  regression script for path formatting rules.

### IAM policy variables (proxy)

- Policy variables such as `${username}` and `${access_key_id}` in resource
  patterns and `s3:prefix` conditions, with safe cloning (no glob corruption).
- Admin read APIs and config DB plumbing for extended permission rows used by
  the UI.
- Documentation updates for SigV4, IAM conditions, and declarative IAM.

### Reliability

- Test harness unsets `DGP_BOOTSTRAP_PASSWORD_HASH` and
  `DGP_ADMIN_PASSWORD_HASH` in the child process so developer shells cannot
  override the test config and break admin login tests.

## v0.9.7 — 2026-05-04

### Lifecycle policies, event outbox, and replication operations

- Added lifecycle v2 support for expiring objects, transitioning objects
  between storage classes/backends, previewing planned actions, and surfacing
  lifecycle state through the admin UI/API.
- Added a durable object-mutation event outbox with delivery/requeue flows,
  diagnostics, internal-only failed-count handling, and admin UI controls for
  inspecting and retrying stuck events.
- Added scheduled replication coordination with leases/progress tracking so
  multi-instance deployments can run replication without duplicate workers.
- Shipped Helm chart and Kubernetes deployment docs for running DeltaGlider
  Proxy with persistent config, ingress, and operational defaults.

### Routing backend and admin UI polish

- Fixed routed-bucket listing semantics so virtual buckets preserve their
  backend origin and duplicate real bucket names follow request-routing
  priority: explicit route, default backend, then stable backend order.
- Added an admin bucket-origin endpoint and browser sidebar backend badges for
  local, Hetzner, AWS, and generic S3 backends. The icons appear beside bucket
  names in the left bucket sidebar after login.
- Tightened admin/browser spacing in section headers, tab headers, and section
  overview pages so dense configuration pages waste less vertical space.

## v0.9.6 — 2026-05-01

## v0.9.5 — 2026-05-01

## v0.9.4 — 2026-04-30

## v0.9.3 — 2026-04-30

## v0.9.2 — 2026-04-30

## v0.9.1 — 2026-04-30

### Marketing site + release docs refresh

- Added a GitHub Pages marketing minisite under `marketing/` with SSG,
  sitemap/robots generation, SEO smoke checks, strict screenshot validation,
  and a dedicated `marketing-pages` workflow.
- Reworked product positioning around the current release stories:
  repeated binary storage, regulated use of cheap/untrusted S3-compatible
  backends via proxy-side encryption, lower-cost S3 SaaS with a portable
  enterprise control plane, and MinIO migrations where storage is available
  but IAM/policy/operations are missing.
- Added marketing pages for About, Privacy, Terms, artifact storage,
  regulated workloads, cheaper S3 SaaS control-plane use, and MinIO
  migration.
- Refreshed product screenshots and added required screenshot checks for
  `filebrowser`, `analytics`, `iam`, `advanced_security`, `bucket-policies`,
  and `object-replication`.
- Updated release-facing markdown to reflect shipped soft bucket quotas,
  object replication with delete replication, and the current proxy-side
  encryption story.

### Fourth-wave correctness fixes (SigV4 integrity + replication provenance + stubs)

Eight findings from the latest review. Two highs are silent
correctness/security failures; the rest tighten honesty in stub
handlers and replication-control endpoints.

**H1 — SigV4 didn't verify the signed payload hash against the body**
A credentialed client could sign hash A and ship body B; the
signature was computed over the canonical-request which only sees
the header value. SigV4's integrity contract requires the receiver
to verify the body downstream — we documented that as a future
guarantee but never implemented it. Fix: the auth middleware
stashes the signed `x-amz-content-sha256` value in a
`SignedPayloadHash` request extension; `put_object_inner` and
`upload_part` recompute the body's SHA-256 and constant-time-compare.
Mismatch → 400 BadDigest. UNSIGNED-PAYLOAD and STREAMING-* sentinels
are recognised and skip verification (they have their own contracts;
see aws_chunked.rs for the streaming variant we don't yet validate
end-to-end). Three integration tests cover signed-mismatch reject,
unsigned-payload pass-through, and the signed-match happy path.

**H2 — replication delete pass deleted unrelated destination objects**
Pre-fix, `replicate_deletes=true` listed every key under
`destination.prefix`, mapped each back to source via prefix-rewrite,
and deleted on source-NoSuchKey. With no provenance marker, an
operator-placed object or a sibling rule's data could be wiped.
Fix: every replicated object carries a `dg-replication-rule = <name>`
user-metadata key. The delete pass only considers candidates whose
metadata carries THIS rule's marker; HEAD-fallback is used when the
listing didn't surface user-metadata; HEAD failure preserves
(false-delete is much worse than a leftover). Integration test seeds
both replicated AND manual objects on destination, runs replication,
then deletes a source key; verifies the replicated key gets deleted
on the next run while the manual one survives.

**M1 — pause/resume created ghost rule rows**
Both handlers called `replication_ensure_state` BEFORE looking up
the rule in config, so a request for a non-existent rule inserted
a state row even though the response was 404. Fix: new
`rule_in_config()` helper runs first; ensure_state only fires
when the rule actually exists. Test verifies that 404 on a ghost
rule leaves the overview clean (no orphan row).

**M2 — run-now bypassed enabled flags**
`replication.enabled=false` and `rule.enabled=false` were honoured
by the (future) scheduler but admin-triggered run-now ignored
both. Fix: explicit gates that 409 with a descriptive message.
Two tests cover the global and per-rule cases.

**M4 — ACL/versioning PUT stubs returned fake 200**
`PUT /bucket?acl`, `PUT /bucket?versioning`, and `PUT /bucket/key?acl`
returned 200 OK while silently discarding the request body. Clients
believed grants/versioning had been applied. Fix: all three return
501 NotImplemented (preceded by a bucket/object existence check so
404 wins when the target is missing). Five integration tests cover
both 501-on-existing and 404-on-missing.

**L1 — tagging PUT/DELETE returned 501 even on missing target**
The previous wave fixed GET ?tagging precedence; PUT/DELETE were
still 501-first. Fix: each calls head/head_bucket before returning
501 so NoSuchKey/NoSuchBucket wins. Three tests cover PUT-on-missing,
DELETE-on-missing, and DELETE-on-existing-object (still 501).

**M3 verified intact** — bucket-subresource existence checks (?location,
?versioning, ?uploads) added in the previous wave still in place.

**L2 verified intact** — `size_is_known = file_size > 0 || !md5.is_empty()`
discriminator in `build_object_headers` correctly emits
`Content-Length: 0` for known-zero managed objects.

Tests: 7 new integration in `replication_test.rs` and `s3_correctness_test.rs`,
3 new auth integration tests in `auth_integration_test.rs`. Full
suite: 659 lib + 28 s3_correctness + 8 replication + 28 auth +
existing suites green. Clippy clean.

### Third-wave correctness fixes (replication + S3 conditionals + headers)

Nine findings from a follow-up review of the replication v1 commits
plus a couple of latent bugs in PUT / bucket subresource paths.

**H1 — replication only copied the first page**
`run_rule` listed one page with `batch_size`, copied it, finished
`succeeded`, and never resumed. A bucket with 50k keys and
`batch_size=100` was 99.8% unreplicated while the dashboard reported
success. Fix: full pagination loop. Per-page `continuation_token`
persisted via `replication_set_continuation_token` so a crash mid-
run resumes from the last batch. Cleared on a clean complete pass.
Bound `MAX_PAGES_PER_RUN = 10_000` defends against pagination
loops in pathological backends. Integration test seeds 17 objects
with `batch_size=5` (≥4 pages).

**H2 — replicate_deletes was config-only, no implementation**
The flag was validated, documented, and ignored. Fix: new
`run_delete_pass` runs after the forward copy when
`rule.replicate_deletes`. Paginates the destination prefix, HEADs
each key on source, deletes destination on `NoSuchKey` source.
Other source-HEAD errors preserve the destination key (false-
delete is worse than a leftover). Skips the delete pass when the
forward pass hit a fatal error (otherwise a transient list
failure could trigger a destination-wide wipe). Integration test
seeds an orphan and verifies it's deleted while legitimate keys
survive.

**H3 — replication lost multipart-ETag identity**
`copy_one` always called plain `engine.store`, so a destination
HEAD returned a fresh full-body MD5 instead of the source's
`"abc-N"` multipart format. Fix: when source's
`FileMetadata.multipart_etag` is `Some`, route through
`engine.store_with_multipart_etag` (the H1 helper from the prior
wave). Integration test creates a real multipart upload on
source, replicates, and asserts source/destination ETags match.

**M1 — replication status="succeeded" with partial failures**
Pre-fix the status only flipped to `failed` when EVERY copy
errored, so dashboards reading `last_status` got a silent partial
failure. Fix: status is `failed` whenever ANY object copy or
delete errored (or a fatal list/planner error happened); else
`succeeded`. The per-object failure ring still captures the full
error list; the status string is now just an honest summary.

**M2 — PUT Object ignored conditional headers**
`If-Match` / `If-None-Match` / `If-Modified-Since` /
`If-Unmodified-Since` were silently dropped, breaking
compare-and-swap and the canonical `If-None-Match: *` idempotent-
create primitive. Fix: new `evaluate_put_conditionals` runs
BEFORE `engine.store` and 412s on any precondition failure. AWS
PUT semantics: all four return 412 (no 304 — that's GET-only).
Two integration tests cover idempotent-create + CAS.

**M3 — bucket subresources answered for ghost buckets**
`GET ?location` / `GET ?versioning` / `GET ?uploads` returned 200
for buckets that didn't exist. Fix: new `require_bucket_exists`
helper at the top of each subresource branch. Three integration
tests pin the 404 contract.

**M4 — tagging stubs returned 501 even for missing resources**
The 501-NotImplemented stubs at GET `?tagging` (object + bucket)
swallowed the head-fail with `let _ = ...; // drain error` and
returned 501 unconditionally. AWS returns 404 first in this case.
Fix: propagate the head error so 404 NoSuchKey/NoSuchBucket wins.
Two integration tests pin the precedence.

**L1 — zero-byte managed objects omitted Content-Length: 0**
`build_object_headers` only emitted Content-Length when
`file_size > 0`, conflating "unknown streaming size" with "known
zero". Clients waiting for the response body got hung up on the
missing terminator for empty managed objects. Fix: new
discriminator `size_is_known = file_size > 0 || !md5.is_empty()`.
Managed objects always carry the canonical empty-MD5
(`d41d8cd98f00b204e9800998ecf8427e`) for empty content, so the
known-zero branch now emits `Content-Length: 0` while the
unknown-streaming branch (empty md5 + file_size = 0) still
omits it for chunked-transfer compatibility.

**L2 — copy-source conditional combos drifted from AWS spec**
AWS S3 CopyObject pairs `if-match` ↔ `if-unmodified-since` and
`if-none-match` ↔ `if-modified-since` with **positive-header
precedence**: when if-match passes, the date check is ignored
entirely. Pre-fix, our linear evaluator rejected requests AWS
would have accepted. Fix: explicit pair-aware evaluator —
if-match present → if-unmodified-since suppressed; if-none-match
present → if-modified-since suppressed. Solo-header behaviour
unchanged. Integration test verifies the if-match-passes-with-
stale-if-unmodified-since combination.

Tests (3 unit + 11 integration new): all under
`src/replication/worker.rs`, `tests/replication_test.rs`,
`tests/s3_correctness_test.rs`. Full suite:
659 lib tests + all integration green. Clippy clean.

### Lazy bucket replication (v1: run-now via admin API)

First cut of scheduled source→destination replication. Built around
the invariant that all copies route through `engine.retrieve` →
`engine.store`, so replication is transparent to per-backend
encryption and delta compression. Operators author rules in YAML
(`storage.replication.rules[]`); runtime state (progress, history,
failures) lives in the config DB.

Scope of this commit stream:

- **Config shape**: new `ReplicationConfig` / `ReplicationRule` /
  `ReplicationEndpoint` / `ConflictPolicy` types with full YAML
  round-trip. Static validation (rule-name regex, humantime
  interval parsing, minimum intervals, batch-size bounds, glob
  compilation) + multi-hop cycle detection in `Config::check`.
- **Pure planner**: `rewrite_key` + `should_replicate` + `plan_batch`
  in `src/replication/planner.rs` — I/O-free, heavily unit-tested
  (15 tests covering the conflict matrix, glob filtering, DG-internal
  skip, directory-marker skip).
- **Persistent state** (`ConfigDb` schema v6): new
  `replication_state` / `replication_run_history` /
  `replication_failures` tables. Boot-time `reconcile_on_boot`
  sweeps zombie `status='running'` rows from a prior crashed
  process. Rules removed from YAML CASCADE-delete their history.
- **Worker**: `src/replication/worker.rs::run_rule` — executes one
  pass of a single rule. Takes `Arc<Mutex<ConfigDb>>` and
  reacquires the lock at each sync boundary so the handler future
  stays `Send` (rusqlite's `Connection` is `!Sync`).
- **Admin API** (session-gated, not IAM-gated):
    - `GET  /_/api/admin/replication` — overview of all rules + state
    - `POST /_/api/admin/replication/rules/:name/run-now` — trigger
      a synchronous run; returns 409 Conflict on a paused rule
    - `POST /_/api/admin/replication/rules/:name/pause` + `/resume`
    - `GET  /_/api/admin/replication/rules/:name/history?limit=N`
    - `GET  /_/api/admin/replication/rules/:name/failures?limit=N`

YAML shape:

```yaml
storage:
  replication:
    enabled: true
    tick_interval: "30s"
    max_failures_retained: 100
    rules:
      - name: prod-to-backup
        enabled: true
        source: { bucket: prod-artifacts, prefix: "" }
        destination: { bucket: backup-artifacts, prefix: "" }
        interval: "15m"
        batch_size: 100
        replicate_deletes: false        # default
        conflict: newer-wins            # newer-wins | source-wins | skip-if-dest-exists
        include_globs: []
        exclude_globs: [".dg/*"]
```

**Scope that landed in follow-up commit streams:**
- Background scheduler loop, periodic ticks, continuation-token
  resumption of long runs, and graceful shutdown.
- Delete replication (`replicate_deletes: true`) with provenance
  markers so only objects written by the same rule are delete
  candidates.
- Admin UI panel for object replication. Multi-hop copies remain out
  of scope.

Dependency added: `humantime = "2"`.

Tests: 15 unit (planner) + 8 unit (state_store) + 2 unit (worker)
+ 2 integration (end-to-end via spawned proxy). Clippy clean.

### Second-wave correctness fixes (H1/H2 + M1–M4 + L1)

Seven additional findings from a follow-up review of the multipart,
copy, tagging, and pagination surfaces.

**H1 — Multipart ETag was inconsistent across Complete vs HEAD/LIST**
CompleteMultipartUpload returned `"md5(concat)-N"` but the persisted
FileMetadata carried a fresh full-body MD5. Clients caching the
Complete ETag as an If-Match precondition hit 412 on the next write.
Fix: new `FileMetadata.multipart_etag: Option<String>` threaded
through `engine.store_with_multipart_etag` / `store_passthrough_
chunked_with_multipart_etag`; `etag()` returns the override when set.
Persisted on both backends (xattr round-trip, S3 user-metadata
`dg-multipart-etag`).

**H2 — DeleteBucket ignored active multipart uploads**
DeleteBucket returned 204 with MPU state still alive for that bucket.
After the delete, ListMultipartUploads reported ghost uploads and
UploadPart (M1 below) silently accepted bytes into the orphan state.
Fix: `MultipartStore.count_uploads_for_bucket` gate in the handler;
409 BucketNotEmpty with MPU wording in the error message.

**M1 — UploadPart / UploadPartCopy didn't check bucket existence**
The C2 wave added `ensure_bucket_exists` to Initiate and Complete but
missed the in-between part upload paths. Attacker could Initiate, have
the bucket deleted, and keep feeding parts. Fix: both UploadPart and
UploadPartCopy now call `ensure_bucket_exists` (UploadPartCopy also
checks the source bucket).

**M2 — CopyObject ignored x-amz-copy-source-if-\* preconditions**
Pre-fix, the four copy-source headers were read from the request but
never evaluated — clients saying "copy only if source is still vX"
got unconditional copies. New pure
`check_copy_source_conditionals(headers, source_metadata)` evaluates
`if-match` / `if-none-match` / `if-modified-since` /
`if-unmodified-since` per AWS spec (all violations return 412, even
the none-match/modified-since variants that would normally be 304
on GET). Wired into both `copy_object_inner` and `upload_part_copy`.

**M3 — invalid x-amz-metadata-directive silently became COPY**
Any value other than case-insensitive `REPLACE` fell through to
`COPY`. A typo like `REPLAC` succeeded with source metadata
preserved — the opposite of the client's intent. Now rejected with
`InvalidArgument` citing the bad value.

**M4 — tagging stubs returned fake 200 while discarding tag state**
Object and bucket PUT/GET/DELETE ?tagging used to return 200/empty
`<TagSet/>`, silently dropping tags the client attached. Clients
using tags for lifecycle, compliance labels, or ABAC would read them
back empty and assume something wiped them. All five handlers now
return 501 NotImplemented — honest "not supported."

**L1 — ListParts / ListMultipartUploads pagination lied**
`max_parts` and `max_uploads` were hardcoded to 1000 with
`is_truncated=false` and empty markers, regardless of input. An
upload with >1000 parts silently dropped the tail. Fix: full
pagination support — `ObjectQuery` + `BucketGetQuery` accept
`max-parts`/`part-number-marker` and
`max-uploads`/`key-marker`/`upload-id-marker`. New paginated helpers
`MultipartStore.list_parts_paginated` + `list_uploads_paginated`
implement tuple-cursor semantics matching AWS.

**Tests**: 5 unit (types) + 2 integration (multipart_etag_test) for
H1, plus 10 integration tests in `tests/s3_correctness_test.rs`
covering H2/M1/M2/M3/M4/L1. 618 lib tests + all integration green.
Clippy clean.

### Security hardening — adversarial review findings (C1–C4 + E1/E2/E4)

A static adversarial review surfaced four critical vulnerabilities and
four edge-case hazards. Every confirmed finding is fixed with
regression tests. No breaking changes for well-behaved clients;
behavioural tightening on attack paths.

**Critical — C1: IAM LIST bypass**
A user with policy `{ resources: ["bucket/alice/*"] }` calling
`GET /bucket?list-type=2&prefix=` previously received every key in
the bucket, because the middleware's `can_see_bucket` fallback
admitted the request and the handler returned unfiltered engine
output. Fix: new `ListScope` request extension marks LIST
authorisations as `Unrestricted` or `Filtered { user }`. When
Filtered, the handler post-filters each key and CommonPrefix
through `user.can(Read|List, bucket, key)`. Unscoped admins pay no
filter cost. A `x-amz-meta-dg-list-filtered: true` response header
signals filtered pages. 10 unit + 5 integration tests pin the matrix.

**Critical — C2: implicit bucket creation via PUT**
`PUT /new-bucket/key` on the filesystem backend previously
succeeded, silently creating the bucket directory as a side effect
of `ensure_dir` → `create_dir_all`. Bypassed `s3:CreateBucket`
equivalence and diverged from the S3 backend contract. Fix: handler
precheck `ensure_bucket_exists` on PUT / COPY (both ends) / Create
+ Complete multipart; backend-level `require_bucket_exists` refuses
writes when the bucket root is missing. 9 tests (5 unit + 4
integration) including filesystem-level "no directory was created"
assertions.

**Critical — C3: multipart in-memory DoS**
`upload_part` previously accepted arbitrarily many parts of
arbitrary size, capped only at Complete-time. Attacker opens
`DGP_MAX_MULTIPART_UPLOADS` (default 1000) uploads and fills each
— process OOMs. Three-layer mitigation: per-upload size cap
(cumulative rejects over `max_object_size`), global in-flight byte
counter (configurable via `DGP_MAX_TOTAL_MULTIPART_BYTES`,
defaulting to `max_object_size * max_uploads / 4`), and idle-TTL
sweeper (configurable via `DGP_MULTIPART_IDLE_TTL_HOURS`, default
24h). Sweeper preserves `Completing` uploads regardless of age.

**Critical — C4: multipart complete/abort race**
Pre-fix, `complete()` set a `completed: bool` under a write lock
then released; the handler then awaited `engine.store*`. During
that await, `abort()` happily removed the upload and returned 204
even though the object was about to land. Fix: replace with a
2-state `MultipartState::{Open, Completing}` enum; abort returns
`InvalidRequest` when state is Completing; handler now calls
`finish_upload` on store success or `rollback_upload` on failure.
8 unit tests cover every legal and illegal transition.

**Hygiene — E1: recursive DELETE no longer materialises full listing**
`DELETE /bucket/prefix/` (trailing slash) previously called
`list_objects(..., u32::MAX, ...)`, materialising the full prefix
in memory before deleting anything. Fix: paginate in 1000-key
windows. Memory stays O(page_size) regardless of prefix depth.
Integration test seeds 1100 objects to exercise the second page.

**Hygiene — E2: conformance marker for `get_passthrough_stream_range`**
The trait's default impl buffers the full object — correct as a
fallback, disastrous as the hot path for a ranged GET. Added a
source-walk conformance test that fails if any in-tree
`impl StorageBackend for X` block omits the override. Prevents a
new backend (future B2, GCS, etc.) from silently inheriting the
default.

**Hygiene — E4: sanitise internal error text to S3 clients**
Backend errors like `StorageError::Other` (with filesystem paths or
MinIO debug strings) and `EngineError::ChecksumMismatch` (with
computed + expected SHA hashes) used to get stringified into error
XML. Fix: new `sanitise_for_client` helper returns a generic
"Internal server error" to the client while logging full detail to
`tracing::error!` under a `dgp::sanitised_error` target.

Env-var knobs added:
- `DGP_MAX_TOTAL_MULTIPART_BYTES` (bytes, defaults to
  `max_object_size * max_uploads / 4`)
- `DGP_MULTIPART_IDLE_TTL_HOURS` (hours, default 24)

**Tests**: 613 lib tests + 19 new integration tests green. Clippy
clean.

**Deferred to a follow-up PR**: E3 (SigV4 chunk-signature
verification) ships under a feature flag.

### Declarative IAM — quality pass + convenience endpoints

Follow-up to the initial 3c.3 shipment (see below). Two reviews
(clean-code hygiene + correctness x-ray) surfaced 10 findings; the
real ones are fixed with regression tests, and two long-promised
operator-convenience endpoints land here:

**Correctness fixes**

- **Critical — mapping_rules wipe on idempotent re-apply**
  (`src/config_db/declarative.rs`). The old `Vec` + ambiguous helper
  couldn't distinguish "YAML matches non-empty DB, keep" from "YAML
  empty, wipe DB". Replaced with an explicit `MappingRulesAction::
  {Keep, ClearAll, ReplaceWith(Vec)}` enum set by `diff_iam`. Before
  the fix, every GitOps reconcile loop on a non-empty rule set
  silently wiped the table — next OAuth login had no mappings.
  Regression tests pin the tri-state matrix.

- **High — `permissions_equal` now normalises case before compare**.
  YAML authored with `effect: "allow"` used to mark every user as
  changed on every apply (DB stores canonical `"Allow"`). The diff
  now normalises both sides first.

- **High — misleading docs on `${env:NAME}` syntax**. The docs
  claimed env-var substitution worked; no implementation exists.
  Docs amended to reflect reality ("plaintext or materialise from
  secret manager at deploy time"); env-substitution stays on the
  roadmap for a later phase.

- **Medium — access-key swap validation**. A YAML that swaps
  access_keys between two surviving DB users now surfaces as a
  clean validation error ("user 'alice' collides on access_key_id
  with existing DB user 'bob'") instead of an ugly mid-transaction
  SQLite UNIQUE failure.

- **Medium — short-circuit step 4c on unrelated PATCHes**.
  `apply_config_transition` used to run the full reconcile +
  `rebuild_iam_index` + `trigger_config_sync` on every PATCH in
  declarative mode, even ones that only touched `log_level` or
  `cache_size_mb`. Now short-circuits when the IAM fields are
  unchanged from the previous config.

- **Low — no-op reconcile no longer triggers S3 sync upload** or
  surfaces a spurious "reconciled:" warning. Idempotent GitOps
  loops now leave the sync bucket alone.

**Hygiene**

- `ReconcileStats::audit_entries() + summary_line()` collapse a
  10-block audit-log loop + two independent format strings into
  single helpers.
- `replace_group_permissions` + `replace_user_permissions` are
  now 3-line delegates to a shared `replace_permissions(tx, table,
  fk, owner_id, perms)`.
- Vestigial `mapping_rules_need_clearing` helper (the bug's
  surface) is gone.

**Operator convenience**

- **Reconciler preview on `/validate`**. `POST /_/api/admin/config/
  section/access/validate` with a declarative-mode body now
  returns a preview warning line: `"declarative IAM preview:
  users(+1/~2/-0) groups(+0/~1/-0) providers(+0/~0/-0)
  mapping_rules=keep"`. The admin-UI's ApplyDialog surfaces it
  under Warnings. Runs the same `diff_iam` the live apply does
  — preview can't drift from reality. Also previews the
  empty-YAML gate refusal so operators catch the problem at
  dry-run time.

- **Export-as-declarative endpoint**. `GET /_/api/admin/config/
  declarative-iam-export` returns a self-contained `access:`
  YAML fragment with `iam_mode: declarative` + populated
  `iam_users` / `iam_groups` / `auth_providers` /
  `group_mapping_rules` projected from the current DB. Secrets
  redacted (operator wires via env). Roundtrip contract:
  exported YAML (with secrets re-injected) → PUT is an
  idempotent no-op. Makes Workflow A ("import existing DB into
  GitOps") a one-button operation instead of hand-assembly.

580 unit tests + 10 declarative integration tests + all encryption
integration tests green. Clippy `-D warnings` clean. Rustfmt clean.

### Declarative IAM reconciler (Phase 3c.3)

`iam_mode: declarative` now actually reconciles. Previously it was a
pure lockout (admin-API IAM mutations returned 403, but nothing synced
YAML → DB). The reconciler runs on every `/config/apply` or
section-PUT on `access` when the target mode is Declarative:

- **Pure diff first, side effects second.** `diff_iam` validates the
  YAML (unique names, valid group refs, valid permissions, no
  access-key collisions, no reserved `$`-prefixed names) and computes
  creates / updates / deletes. Any validation failure returns an
  error with ZERO DB writes.
- **Atomic apply.** `apply_iam_reconcile` runs all mutations in a
  single SQLite transaction. Partial failures roll the whole
  reconcile back; state never observed mid-apply.
- **ID preservation.** Users and groups are matched by NAME, not by
  DB id. Rotating an access key in YAML is an UPDATE that preserves
  the row id — so `external_identities` linked to that user stay
  valid. `external_identities` themselves are NOT reconciled (they
  are runtime OAuth byproducts) but ARE cascade-deleted when a
  YAML-authoritative delete removes the user or provider they
  reference.
- **Empty-YAML gate.** A `gui → declarative` flip with empty
  `iam_users` / `iam_groups` is refused — it would wipe the DB.
  Declarative → declarative with empty YAML is allowed
  (operator deliberately clearing all IAM).
- **Audit trail.** Every mutation emits an `iam_reconcile_*` audit
  entry (`_user_create` / `_update` / `_delete`, same for groups
  and providers, plus `_mapping_rules_replaced`).

Wire shape: `access.iam_users` / `iam_groups` / `auth_providers` /
`group_mapping_rules` on `AccessSection`. Users reference groups by
NAME; mapping rules reference providers and groups by NAME. IDs are
ephemeral and must not appear in YAML.

See `docs/product/reference/declarative-iam.md` for the full
walkthrough (including both workflows — importing an existing
populated DB, or authoring fresh IAM from YAML — and the complete
adversarial-edge table).

### Backends panel: legacy singleton now surfaces correctly

`GET /api/admin/backends` used to return an empty list when the
config used the legacy `storage.backend` singleton (vs the named
`storage.backends` list), so the admin UI showed "No named backends.
Using legacy single-backend mode." while the proxy was actively
serving. `GET /api/admin/config` already synthesised a `"default"`
entry to keep the UI functional elsewhere; `/backends` now does the
same.

Synthesised entries carry `is_synthesized: true` in the response so
the UI can disable destructive actions (they don't correspond to a
real `cfg.backends[]` row; `DELETE` would 404). The Backends panel
renders them with a "LEGACY SINGLETON" badge and no Delete button.

## v0.8.10

A QA-focused release. No user-facing behaviour changes in the S3 API
or delta codec. Two new admin endpoints, targeted new coverage for
hot paths, and a sharper test suite overall.

### New admin affordances

- **`POST /api/admin/config/sync-now`** — operator-triggered pull
  from the config-sync S3 bucket. Previously only reachable by
  waiting up to 5 minutes for the periodic poll. Useful for
  multi-replica deployments wanting immediate IAM propagation after
  an out-of-band mutation. Returns 404 when `config_sync_bucket` is
  unset, 200 with `{downloaded, status}` otherwise.
- **`GET /api/admin/iam/version`** — monotonic counter bumped on
  every IAM index rebuild (user/group CRUD, OAuth provider change,
  mapping-rule edit). Public on purpose — exposes only an opaque
  number. Powers the `wait_for_iam_rebuild` test barrier and is
  generally useful for scripting "wait for propagation."

### Shared helper moved into the library

`reopen_and_rebuild_iam` relocated from `src/startup.rs` (binary)
to `src/config_db_sync.rs` (library) so the admin `sync-now`
handler can reach it without duplicating DB-reopen + IAM-rebuild
logic. Behaviour unchanged; callers (startup, periodic poller,
sync-now) funnel through one path.

### Test posture overhaul (internal)

Not user-visible but worth recording because it affects CI + dev
loop durability:

- **Deterministic IAM rebuild barrier** (`wait_for_iam_rebuild`)
  replaces two `tokio::time::sleep(1s)` calls in
  `auth_integration_test.rs`. Auth suite went from ~3.5s to ~1.5s
  and is no longer flake-prone on slow CI runners.
- **6 new unit tests** for `classify_s3_error` / `classify_get_error`
  in `storage/s3.rs` — zero unit tests before, now covers the
  Hetzner/Ceph 403-for-missing-bucket quirk, object-level 403
  preservation, and NoSuchKey → NotFound mapping.
- **7 `test_create_bucket_*` integration tests replaced by 3 unit
  tests** in `handlers/bucket.rs` + property-based coverage via
  `proptest` (new dev-dep). ~1500 random cases per run, 0.08s.
- **9 S3-backend duplicate tests deleted** from
  `tests/s3_backend_test.rs` (verified trait behaviour already
  covered by the filesystem-backed suites). Kept 2: SDK-plumbing
  smoke + real delta+S3 interaction.
- **6 of 10 public-prefix LIST integration tests removed** — unit
  tests in `api/auth.rs` and `admission/evaluator.rs` already cover
  the policy-match shape. Kept: no-slash AWS-CLI bug regression,
  false-parent denial, partial-public root denial, admin sees all.
- **2 new integration tests for metadata-cache invalidation** in
  `tests/optimization_test.rs` — covers PUT-overwrite and batch-
  delete paths that the docstring pinned but no test exercised.
- **4 new HA config-sync tests** (`tests/config_sync_ha_test.rs`)
  exercising the real sync code path: startup pull, sync-now
  propagation, ETag no-op, wrong-passphrase rejection.
- **3 new same-key concurrency tests** (`tests/concurrency_test.rs`)
  for double-`CompleteMultipartUpload`, cross-upload-ID isolation,
  and PUT-vs-DELETE state consistency.
- **3 new SigV4 security tests** (`tests/auth_integration_test.rs`)
  covering signed-header tampering, presigned-URL rejection after
  user disable, and unsigned-extra-header tolerance (spec
  compliance).
- **5 new unit tests for `src/startup.rs`** (was 0/650 LOC).
- **Coverage reporting in CI.** New `coverage` job runs
  `cargo-llvm-cov --lib` and publishes a summary table +
  lcov.info artifact. `continue-on-error: true` — signal, not a
  gate.
- **TestServer hardening.** `wait_ready` now checks
  `process.try_wait()` BEFORE the health probe, so a stray proxy
  holding the test port fails loudly (`lsof -i :<port>`) instead
  of silently intercepting requests.

### Internal hygiene

- **Shared `useSectionEditor` hook** in the admin UI collapses
  ~450 LOC of triplicated apply-flow boilerplate across
  AdmissionPanel, CredentialsModePanel, and the Advanced sub-
  panels into one ~220 LOC hook. Future section panels plug in
  rather than re-carrying the §F5 snapshot-at-validate fix etc.
- **`with_config_db()` wrapper** in `src/api/admin/mod.rs` shrinks
  the "get DB from state → lock → run op → log-and-500" admin
  handler boilerplate from 5–8 lines to 3. Migrated 8 handlers
  in `external_auth.rs`.
- **6 duplicate `default_*` fns** in `config_sections.rs`
  eliminated by making the `config.rs` versions `pub(crate)`.
  Round-trip test continues to guard against drift.

### Documentation

- **`CLAUDE.md` refreshed** against recent architecture drift.
  Two new sections: "Testability principles" (7 bullets with
  prior-art citations) and "When proposing architecture" (4
  anti-patterns to avoid).

### Metrics

- **Test count**: 467 → 487 unit/property/binary tests. Integration
  tests net −12 (22 deleted as redundant, 10 added for genuine
  gaps).
- **CI time**: neutral — slower startup-unit-test overhead (~0.5s)
  offset by ~2s saved in auth + bucket-name tests.

## v0.8.0

A major release. Two overlapping threads land together: the
**progressive-disclosure YAML config** (phases 0–3 of
[docs/plan/progressive-config-refactor.md](docs/plan/progressive-config-refactor.md))
and the first waves of the **admin UI revamp**
([docs/plan/admin-ui-revamp.md](docs/plan/admin-ui-revamp.md)).

### Configuration (progressive-disclosure YAML — phases 0–3)

- **YAML is now canonical.** `deltaglider_proxy.yaml` (four-section
  `admission / access / storage / advanced` layout) is preferred
  over `deltaglider_proxy.toml`. Both still load; TOML is
  deprecated with a `tracing::warn!` on every load (silence with
  `DGP_SILENCE_TOML_DEPRECATION=1`). See
  [docs/HOWTO_MIGRATE_TO_YAML.md](docs/HOWTO_MIGRATE_TO_YAML.md).
- **Dual-shape loader.** Sectioned YAML (`admission:`/`access:`/
  `storage:`/`advanced:`) is transparent to the in-memory
  `Config` struct — the flat shape still loads unchanged.
- **Storage shorthand** — a single-backend deployment can write:
  ```yaml
  storage:
    s3: https://example.com
  ```
  which expands to a full `backend: { type: S3, ... }` at load
  time. Filesystem shorthand (`storage: { filesystem: /path }`)
  likewise. Long-form YAML stays valid; shorthand is operator
  convenience only.
- **Per-bucket `public: true`** shorthand expands to
  `public_prefixes: [""]`, and the canonical exporter collapses
  back when unambiguous. GUI "Public read" toggle maps 1:1 to the
  YAML.
- **Admission chain (operator-authored).** New
  `admission.blocks[]` wire format with `match` predicates
  (method / source_ip / source_ip_list / bucket / path_glob /
  authenticated / config_flag) and `action` variants
  (`allow-anonymous`, `deny`, `reject { status, message }`,
  `continue`). Evaluator dispatches live before the synthesized
  public-prefix blocks; reserved `public-prefix:*` name prefix;
  glob compile-check at parse time; 4096-entry cap on
  `source_ip_list`.
- **`access.iam_mode: gui | declarative`.** New lifecycle knob:
  `gui` (default) keeps the encrypted IAM DB as source of truth;
  `declarative` gates admin-API IAM mutation routes (the
  `require_not_declarative` middleware) behind 403 responses. A
  warn-level audit log line fires on every mode transition. The
  reconciler that sync-diffs DB to YAML in declarative mode is
  still Phase 3c.3; today declarative mode is a read-only
  lockout.
- **New admin endpoints** for GitOps + GUI round-trips:
  - `GET /api/admin/config/export[?section=<name>]` —
    canonical-YAML export with every secret redacted; scope-filter
    to one section when `section=` is present.
  - `POST /api/admin/config/validate` — dry-run a full YAML doc.
  - `POST /api/admin/config/apply` — atomic full-document apply,
    with runtime-secret preservation for redacted round-trips.
  - `POST /api/admin/config/trace` — evaluate a synthetic request
    against the live admission chain.
  - `GET /api/admin/config/defaults[?section=<name>]` — JSON
    Schema (via `schemars`) for the Config type, optionally scoped
    to one section for Monaco's per-editor schema.
- **Section-level admin API (Wave 1 of the UI revamp):**
  - `GET /api/admin/config/section/:name[?format=yaml]`
  - `PUT /api/admin/config/section/:name` — partial update,
    routes through the same `apply_config_transition` helper as
    field-level PATCH and document-level APPLY.
  - `POST /api/admin/config/section/:name/validate` — dry-run
    with a diff body (`{section: {field.path: {before, after}}}`)
    for the plan → diff → apply dialog.
  - `GET /api/admin/config/trace` — query-param variant for
    bookmarkable trace URLs.
  - `GET /api/admin/audit?limit=N` — recent entries from the
    in-memory audit ring (newest first; limit clamped to
    `[1, 500]`). Backs the Diagnostics → Audit log viewer
    (Wave 11).
- **CLI subcommands:**
  - `deltaglider_proxy config migrate <toml>` — TOML → YAML
    converter (emits canonical sectioned form).
  - `deltaglider_proxy config lint <yaml>` — offline schema +
    reference-resolution + dangerous-default warnings.
  - `deltaglider_proxy config defaults` — dump every default
    with its doc comment.

### Admin UI revamp (waves 1–11)

All ten waves from
[docs/plan/admin-ui-revamp.md](docs/plan/admin-ui-revamp.md) plus
a follow-on Wave 11 (audit log viewer) have landed as of this
release. Waves 1–3 shipped at tag time; waves 4–8 landed during
live-browser verification; waves 9 (Trace diagnostics), 10
(command palette + shortcuts help), 10.1 (finish §10 polish
items), and 11 (audit log viewer) shipped post-tag and are
rolled into the v0.8.0 entry so the `main` history is
self-describing. §9.1's Dashboard redesign was deferred — the
existing MetricsPage remained functional so the rewrite can
ship independently without blocking operators today.

- **Section-level API client helpers in `adminApi.ts`**:
  `getSection`, `putSection`, `validateSection`, `getSectionYaml`,
  `exportSectionYaml`, `getSectionSchema`, `getFullConfigSchema`.
- **New foundation components** ready for use in waves 4–7:
  - `FormField` — standardised label / YAML-path / help /
    default-placeholder / override-indicator / owner-badge wrapper.
  - `ApplyDialog` — the plan → diff → apply modal (§5.3).
  - `MonacoYamlEditor` — lazy-loaded Monaco + monaco-yaml with
    scoped JSON Schema and mobile fallback (§4.3, §10.4).
- **New hook `useDirtySection`** — per-panel dirty state backed
  by a module-level Set, with `useDirtyGlobalIndicators` for the
  `● ` tab-title prefix and `beforeunload` guard.
- **New sidebar** (`AdminSidebar`) — four-group IA
  (Diagnostics + Configuration). Nested sub-entries for Access /
  Storage / Advanced; amber dot on sections with unsaved edits.
- **URL scheme** — every admin page is a bookmarkable
  hierarchical URL:
  `/_/admin/diagnostics/dashboard`,
  `/_/admin/configuration/access/users`, etc. Legacy flat URLs
  (`/_/admin/users`, `/_/admin/backends`) keep working via
  `LEGACY_TO_NEW` in AdminPage.tsx.
- **Right-rail actions** (`RightRailActions`) visible on every
  Configuration page: Apply / Discard (gated on `dirty`) and
  Copy YAML (section-scoped). In practice the rail was
  simplified during Wave 3's adversarial review to copy-only —
  section-scoped paste and full-document Import/Export run from
  the `YamlImportExportModal` reached through the header actions.
- **YAML Import/Export modal** (`YamlImportExportModal`) reached
  from every admin page via the header actions. **IAM Backup**
  was renamed in the sidebar (was "Backup") to distinguish it
  from the YAML Import/Export surface: IAM Backup ships the full
  encrypted SQLCipher DB (users, groups, OAuth, mappings); YAML
  Import/Export ships the operator config document.
- **Admission block editor** (Wave 4, `AdmissionPanel`) — drag-
  to-reorder list of operator-authored blocks with Form ⇄ YAML
  toggle per row, inline validation matching the server rules
  (name charset, reserved `public-prefix:*` prefix, mutually
  exclusive `source_ip` / `source_ip_list`, 4xx/5xx `reject`
  status), and synthesized `public-prefix:*` blocks surfaced
  read-only below.
- **Access → Credentials & mode panel** (Wave 5) — the first
  screen on the Access section: iam_mode radio (gui /
  declarative with the Phase 3c.3 gap disclosed inline),
  authentication-mode dropdown, bootstrap SigV4 key pair with
  rotate-in-place, and a Change password link.
- **Storage → Buckets panel** (Wave 6) — per-bucket editor with
  tri-state Anonymous read access (None / Specific prefixes /
  Entire bucket). Selecting "Entire bucket" writes the compact
  `public: true` shorthand; "Specific prefixes" writes an
  explicit `public_prefixes` list. The GUI toggle maps 1:1 to
  the YAML.
- **Advanced panel sub-sections** (Wave 7) — the Advanced
  section is split into five dedicated sub-panels (Listener &
  TLS, Caches, Limits, Logging, Config DB sync), each with
  grouped forms, `🔁` restart-required badging, and monospace
  `from DGP_X_Y` chips on env-var-owned fields.
- **First-run setup wizard** (Wave 8) — five-screen guided
  onboarding at `/_/admin/setup`: backend pick → backend config
  (with live Test Connection for S3) → admin credentials →
  optional public bucket → review + apply. Routes to the
  Dashboard on success. Zero-to-working in under three minutes
  on a fresh install.
- **IAM source-of-truth banner** (`IamSourceBanner`) — surfaced
  on every Access sub-panel: explains in plain English whether
  the listed users/groups/providers are DB-managed (iam_mode:
  gui) or read-only because YAML owns the state (iam_mode:
  declarative).
- **AntD 6 shrink fix** — `theme.css` overrides the AntD 6
  radio/checkbox "shrink on click" default that was breaking
  Wave 4's match-action radio groups.
- **Admission trace panel** (Wave 9, `TracePanel`) — the
  `/_/admin/diagnostics/trace` placeholder is replaced by a real
  admission-chain debugger. Synthetic-request form (method /
  path / query / source IP / authenticated toggle), one-click
  example chips, and a result pane built around the Istio /
  Kiali "reason path" pattern: decision tag, matched-block name,
  resolved-request breakdown (method / bucket / key /
  list_prefix / authenticated), and Copy-as-JSON + Clear
  actions. Calls `POST /api/admin/config/trace` under the hood;
  no dirty state (pure read). A "How to read the output" info
  alert demystifies the panes for first-time users.
- **Command palette** (Wave 10, `CommandPalette`) — `⌘K` /
  `Ctrl+K` opens a fuzzy-filter palette over every entry in the
  four-group IA plus shell-scope actions (Export YAML, Import
  YAML, Setup wizard, Keyboard shortcuts, Back to Browser).
  Hand-rolled scorer (substring + subsequence, <50-line
  function); combobox ARIA with `aria-controls` /
  `aria-activedescendant`; group headings ("Recent" / "Navigate"
  / "Actions") rendered non-interactively so Arrow keys skip
  over them. Recent-items MRU (last 5) persists to localStorage
  and surfaces under the "Recent" heading on empty query.
- **Shortcuts reference modal** (Wave 10, `ShortcutsHelp`) —
  `?` opens a platform-aware shortcut list. Mac users see only
  `⌘` rows; Windows / Linux users see only `Ctrl` rows — no
  duplicate "same as ⌘K on non-Apple" noise rows. Detection via
  `navigator.userAgentData.platform` with `navigator.platform`
  + UA-string fallbacks (`platform.ts`). The listener itself
  still accepts BOTH modifiers so a Mac user on a PC keyboard
  still works.
- **`⌘S` / `Ctrl+S` → Apply dirty section** (Wave 10.1, §10.3).
  `useDirtySection.ts` gains `registerApplyHandler(section, fn)`
  + `useApplyHandler(section, fn, enabled)`; handlers are
  stacked per section so the most-recently-mounted panel wins
  when siblings share a section (Wave 5 master-detail).
  `AdminPage` dispatches `⌘S` via `requestApplyCurrent(
  sectionForPath(adminPath))`; falls through to the browser
  default when no handler is registered (Diagnostics pages, no
  dirty state). Wired into Admission, Credentials & mode, and
  all five Advanced sub-panels.
- **Mobile drawer** (Wave 10.1, §10.4) — below 900px the
  persistent sidebar collapses to an AntD Drawer that slides in
  from the left. Hamburger trigger lives in the header. Drawer
  auto-closes on navigation. `useIsNarrow` hook tracks the
  breakpoint live (rotate / devtools-toggle works without
  reload).
- **i18n scaffold** (Wave 10.1, §10.2, `i18n.ts`) — pass-through
  `t(key, fallback)` helper and matching `useT()` hook. Today
  returns `fallback ?? key` — no translation tables yet.
  Purpose: when we add a locale, we swap one file's internals
  and every existing call site continues to work.
- **Audit log viewer** (Wave 11, `AuditLogPanel`) — the admin UI
  now surfaces the server's audit trail at
  `/_/admin/diagnostics/audit`. Backed by a new in-memory ring
  in `src/audit.rs` (bounded `VecDeque<AuditEntry>`; default 500
  entries; override via `DGP_AUDIT_RING_SIZE`). Every
  `audit_log()` call pushes a sanitised copy onto the ring, so
  stdout / JSON log shippers see nothing change — the ring is
  supplementary. New `GET /api/admin/audit?limit=N` endpoint
  (session-gated; not IAM-gated — all admins see the same log).
  Panel renders a monospace table with colour-coded Action tags
  (red = `login_failed` / `delete`, green = `login`, etc.),
  free-text filter across every column, optional 3-second
  auto-refresh, and an "In-memory ring — not a compliance
  substitute" banner so operators don't mistake it for durable
  storage.
- **IAM Backup import preserves `external_identities`** (post-
  manual-review hardening). `POST /api/admin/backup` used to
  silently drop every OIDC identity binding; restoring a backup
  would orphan `user_id ↔ external_sub ↔ provider_id` links and
  returning OIDC users got re-provisioned as new accounts. Import
  now remaps `user_id` + `provider_id` through the existing ID
  maps and falls back to `groups.member_ids` / autoincrement
  heuristics for legacy backups that lack a `bu.id` field. New
  optional `BackupUser.id` field in exports. Idempotent: repeat
  imports skip existing `(provider_id, external_sub)` pairs.
  `ImportResult` gains `external_identities_created` +
  `external_identities_skipped` counters.
- **GUI polish** (post-manual-review round):
  * Groups create form closes and navigates to the Edit view after
    a successful create. Previously the form stayed open with
    stale fields; "+ New" appeared broken until a page reload.
  * ExtAuth mapping-rule "+ Add Rule" flushes pending edits on
    other rules BEFORE create + reload. Previously the reload
    overwrote the in-memory rules array, silently clobbering
    unsaved typing in sibling rows.
  * New mapping rules start with an empty `match_value` and a
    placeholder hint. Previously pre-filled with `*@example.com`
    as a literal default (forcing select-all + delete before typing).
  * Users list label reflects group inheritance: `N groups
    (inherited)` or `N rules · M groups` instead of a misleading
    "No access" for users who have group-inherited permissions.

### Bug fixes / correctness

- **`DGP_BOOTSTRAP_PASSWORD` env override.** `--config <path>`
  now applies env var overrides (including
  `DGP_BOOTSTRAP_PASSWORD_HASH`) on top of the file, matching the
  behaviour of implicit search-path loading. Previously the flag
  made env vars silently ignored.
- **Admission validator** rejects reserved `public-prefix:*`
  names up front.
- **Shape classifier** explicit check-by-key-presence (flat vs.
  sectioned), so typos inside a sectioned doc report section-
  scoped errors rather than "unknown variant" from a fallback
  parse.
- **Section PUT is RFC 7396 merge-patch** (post-tag regression
  fix from live-browser verification). The first cut of
  `PUT /api/admin/config/section/:name` replaced the whole
  section — a PUT with `{max_delta_ratio: 0.42}` silently reset
  `cache_size_mb`, `listen_addr`, and `log_level` to their
  compile-time defaults. Now: present keys apply in place;
  absent keys preserve the current value; explicit `null`
  reverts to the default; array fields (e.g.
  `admission.blocks`) stay atomic. Three regression tests lock
  this in.
- **`advanced.log_level` from YAML applied at startup** (post-
  tag fix). `init_tracing` previously read only from RUST_LOG
  and DGP_LOG_LEVEL env vars, silently ignoring the YAML value;
  runtime always ran at `debug` default unless env was set.
  Now: RUST_LOG > DGP_LOG_LEVEL > config.log_level > --verbose
  > default. Env-driven deployments (Docker + K8s) keep their
  semantics exactly.
- **Legacy admin URLs canonicalise in the browser bar** (post-
  tag fix). Bookmarked v0.7.x URLs like `/_/admin/users`
  resolved correctly but left the address bar on the legacy
  form, spreading the old shape through copy/paste. A
  `useEffect` in `AdminPage` now swaps in the canonical
  hierarchical URL via `navigate(..., replace=true)` without
  adding a history entry.

### Dependencies

Frontend: `monaco-editor`, `monaco-yaml`, `react-hook-form`, `zod`,
`@hookform/resolvers`, `@dnd-kit/core` + `sortable` + `utilities`
(§4.2–§4.4). Backend: `serde_yaml`, `schemars`, `ipnet`, `globset`.

### Breaking changes

None. TOML config is deprecated but still loads; field-level
`PATCH /api/admin/config` remains the stable GUI surface; every
existing integration test passes.

### Migration

Run `deltaglider_proxy config migrate deltaglider_proxy.toml
--out deltaglider_proxy.yaml`, point the server at the YAML, and
delete the TOML when ready. No config rewrites needed on the
YAML side for existing deployments — the sectioned shape is
semantically a superset of the flat shape.

## v0.7.2

### UI Polish & Usability
- **Presigned URL duration selector**: Share button is now a split button — main button generates a 7-day link, chevron dropdown offers 1 hour, 24 hours, or 7 days. Share modal displays "Expires in" label.
- **Version display**: Sidebar branding shows the proxy version (fetched from whoami, not the unauthenticated health endpoint).
- **OAuth user identity**: Sidebar shows the user's email address (from external identity) instead of the truncated access key ID. Username displayed on its own row for readability.
- **Ant Design tooltips removed globally**: CSS nuke (`display: none !important`) on `.ant-tooltip, .ant-popover` prevents all layout-shaking tooltip bugs. All 6 `<Tooltip>` usages replaced with native `title` attributes.
- **Analytics tab padding fix**: Removed double padding (AnalyticsSection had its own wrapper padding on top of the parent MetricsPage padding). Aligned card styles (borderRadius, fontSize, fontFamily, spacing) between Monitoring and Analytics tabs.

### Correctness
- **Analytics stats**: Stats endpoint now calls `list_objects` with `metadata=true`, so delta compression savings are correctly reported (was always showing 0% savings).
- **InspectorPanel crash on zip files**: Fixed React error #310 — `useState` for share duration was declared after the early return, causing hook count mismatch.
- **Whoami returns user info**: `GET /api/whoami` now resolves the logged-in user from the session cookie (name, access_key_id, is_admin). OAuth users show their email, IAM users show their name, bootstrap shows "admin".

### Security
- **Version removed from health endpoint**: `/_/health` no longer exposes `version` field. Version is only available via the authenticated `/_/api/whoami` endpoint.

### Infrastructure
- **Docker debugging tools**: `ntpstat` and `chrony` pre-installed in the runtime image for clock skew diagnosis.

## v0.7.1

### Features
- **Bulk copy/move**: Select multiple objects and copy or move them to a different bucket/prefix via a destination picker modal. Move = copy + delete source (only after all copies succeed).
- **Bulk download as ZIP**: Select multiple files and download them as a client-side ZIP (fflate). Size warning for selections >500MB.
- **Storage analytics dashboard**: New Analytics tab in Metrics page with summary cards (total storage, space saved, savings %, estimated monthly cost savings), per-bucket stacked bar chart, session savings area chart, and compression opportunity identifier.
- **Cost configuration**: Gear icon on the monthly savings card opens a preset selector (AWS S3, S3 IA, Hetzner, Backblaze, Cloudflare R2) with localStorage persistence.
- **Themed error pages**: OAuth error pages (provider not ready, auth failed, account disabled) respect the user's dark/light theme via CSS custom properties and inline localStorage/prefers-color-scheme detection.

### UI Components
- **BulkActionBar**: Toolbar with Copy, Move, ZIP, Delete buttons — replaces the inline delete button when objects are selected.
- **DestinationPickerModal**: Bucket dropdown + prefix autocomplete, preview of destination paths, move-mode deletion warning.
- **AnalyticsSection**: Summary cards, Recharts bar/area charts, compression opportunity cards.

## v0.7.0

### Features
- **OAuth/OIDC external authentication**: Login with Google (or any OIDC provider) via the admin GUI. PKCE, state parameter, nonce, JWT validation. Per-provider configuration with display names and custom branding.
- **Group mapping rules**: Map external identity claims (email domain, email glob, email regex, claim value) to IAM groups. Rules evaluated on every OAuth login; group memberships merged (not replaced).
- **Mandatory authentication**: Proxy refuses to start without authentication credentials unless `authentication = "none"` is explicitly set. Prevents accidental open-access deployments.
- **Per-bucket compression policies**: Enable/disable compression per bucket in the Backends panel. When per-bucket compression is ON but global ratio is 0, uses 0.75 default.
- **Multi-instance config sync**: ExternalAuthManager rebuilds after S3 config sync. Stale pending OAuth flows cleared on provider rebuild.

### UI
- **OAuth login buttons**: Connect page shows branded OAuth provider buttons with "credentials instead" collapsible.
- **Authentication panel**: Full OAuth provider CRUD, mapping rules with local state + Save button (not per-keystroke API calls).
- **Backends panel**: Merged Storage + Compression tabs. Per-bucket policy toggles inline with backend configuration.
- **Custom dropdowns**: `SimpleSelect` and `SimpleAutoComplete` replace broken Ant Design Select/AutoComplete popups.
- **Tab headers**: Centered, typographically consistent headers for all admin settings tabs.
- **Object table fixes**: `showSorterTooltip={false}` prevents sort header shaking. Native `title` attributes replace broken Ant tooltips.

### Security
- **Session cookie fixes**: SameSite=Lax (not Strict) for OAuth redirect compatibility. Secure flag auto-detects TLS.
- **XSS protection**: `escape_html()` on OAuth error page parameters.
- **Rate limiting on OAuth callback**: Prevents brute-force state token guessing.
- **Group membership merge**: OAuth re-login no longer wipes existing group memberships.

### Bug Fixes
- **Bucket not loading on first click**: `s3client.setBucket()` called before React state update.
- **Empty browser after login**: `s3.reconnect()` moved to useEffect (was in Promise callback before state committed).
- **Upload SignatureDoesNotMatch**: Strip leading/trailing slashes from upload destination path.
- **InternalError hiding real errors**: Error message now shows actual error, not generic text.
- **Reference.bin fallback**: GET on aliased bucket paths falls back to passthrough when reference fetch fails.

## v0.6.0

### Features
- **Multi-backend routing**: Route different buckets to different storage backends (filesystem, S3, mixed). `RoutingBackend` implements `StorageBackend` transparently — zero engine changes, shared caches/codec/locks. Configure via `[[backends]]` TOML array or Admin GUI.
- **Backends admin panel**: New "Backends" tab in Admin Settings. Add/remove named backends, test S3 connections, set default backend. Safety checks prevent deleting in-use or default backends.
- **Bucket routing UI**: Per-bucket policy cards now show Backend + Alias fields when multi-backend is active. Route virtual bucket names to specific backends with optional real-name aliasing.
- **TOML/env config hints**: Read-only settings (Limits, Security, Advanced Compression) now show copyable `TOML:` and `ENV:` examples below each field.
- **Bucket policies promoted**: Per-Bucket Compression section moved to top of Compression tab with improved UX — contextual labels, amber tint on disabled buckets, empty state explains use cases.

### Bug Fixes
- **Dropdown positioning**: Replaced Ant Design `<Select>` with `<Radio.Group>` for backend type chooser. Fixes broken dropdown positioning at non-100% browser zoom (known `@rc-component/trigger` bug).
- **list_buckets_with_dates**: Routed virtual buckets no longer mask real creation dates with `now()`. Backends are queried first; route names added only if not already found.
- **Config validation**: `default_backend` is now validated against the `backends` list at load time. Invalid references are cleared with a warning. Bucket policy backend references are also validated.
- **S3 credential validation**: `POST /api/admin/backends` now validates credentials upfront (400) instead of failing at engine rebuild (500).
- **Taint detection**: `compute_tainted_fields` now compares `backends` and `default_backend` between runtime and disk config.

### Code Quality
- **DRY**: `BackendInfoResponse::from()` impl replaces copy-pasted conversion in two files. `rebuild_engine` made `pub(super)` and shared across config.rs and backends.rs. `DEFAULT_CONFIG_FILENAME` constant replaces 6 magic strings.
- **Dead code**: Removed inline save block from backendTab (uses shared `saveSection`). `embeddedTab` prop made required.
- **Replay window**: Hoisted duplicate `DGP_REPLAY_WINDOW_SECS` env parse to single variable.

## v0.5.11

### Features
- **Per-bucket compression**: Configure compression enable/disable and max delta ratio per bucket via TOML (`[buckets.<name>]`), admin API, or GUI. Unconfigured buckets inherit global defaults.
- **Bucket Policies GUI**: New "Bucket Policies" section in Compression tab — add, remove, edit per-bucket overrides with live save.

### Security
- **COPY source path traversal**: Reject `..` in `x-amz-copy-source` bucket and key (filesystem backend).

### Bug Fixes
- **Bucket policy hot-reload**: Fixed case normalization mismatch (config stored original case, engine expected lowercase). Added rollback on engine rebuild failure.

## v0.5.10

### UI & Documentation
- **Path-based routing**: Clean URLs (`/_/browse`, `/_/admin/users`, `/_/docs/configuration`) replace hash routing. Deep-linking to doc pages with heading anchors. Old hash bookmarks auto-redirect.
- **Embedded docs**: Full-text search (minisearch + Cmd+K), Mermaid diagram rendering with getBBox viewBox fix, Lightbox for images, landing page with screenshots and feature cards.
- **Admin GUI**: All 39 settings surfaced across 5 tabs (Backend, Compression, Limits, Security, Logging). Theme toggle in Admin/Docs header.
- **Design system**: 16 new CSS color variables for dark/light themes. Responsive breakpoints (grids collapse on mobile, panels hide).

### Correctness
- **SettingsPage**: handleSave wrapped in try-catch (prevents stuck save button on network error).
- **Multipart**: complete() no longer removes upload before store() succeeds (prevents data loss on transient failure).
- **SigV4**: Credential scope format validation in both presigned and header auth paths.
- **ListObjectsV2**: max-keys=0 returns InvalidArgument per S3 spec.
- **DocsPage**: Fixed stale useCallback dependency in handleLinkClick.

### Infrastructure
- **build.rs**: `rerun-if-changed` for dist/ directory (no more stale embedded UI assets).
- **Dead code**: Removed ApiDocsPage, BrowserConnectionCard, dead hash routes (~400 lines).
- **DRY**: FullScreenHeader shared component, FULLSCREEN_VIEWS set, dedup-by-key unified.

## v0.5.9

### Bug Fixes
- **SigV4 replay**: Fixed false positives — presigned URLs and idempotent methods (GET/HEAD) skip replay detection. Replay window reduced from 300s to 2s.
- **Logging**: SigV4 mismatch logs method, path, scope, signature prefixes. Clock skew logs direction.
- **Config DB**: Added PRAGMA busy_timeout=5000ms on open and reopen.

## v0.5.4

### Security & Hardening

- **Per-request timeout**: Added `tower_http::timeout::TimeoutLayer` returning HTTP 504 Gateway Timeout after 300s (configurable via `DGP_REQUEST_TIMEOUT_SECS`). Prevents slow clients from holding concurrency slots forever.
- **Replay cache cap**: Replay cache capped at 500K entries. If exceeded (flood attack), cache is cleared with a `SECURITY |` warning.
- **Recursive delete IAM enforcement**: Server-side recursive prefix delete (`DELETE /bucket/prefix/`) now checks per-object IAM permissions. Previously bypassed individual Deny rules.
- **Bootstrap hash format validation**: Rejects malformed bcrypt hashes at startup instead of failing silently on first auth attempt.

### Features

- **Server-side recursive delete**: `DELETE /bucket/prefix/` (trailing slash) deletes all objects under the prefix. Per-object IAM checks enforced. Filesystem backend uses native `remove_dir_all`.
- **S3 batch delete**: Batch delete (`POST /?delete`) uses `DeleteObjects` API instead of per-file DELETE for ~10x fewer API calls.
- **Base64 bootstrap password hash**: `DGP_BOOTSTRAP_PASSWORD_HASH` accepts base64-encoded bcrypt hashes to avoid `$` escaping issues in Docker/env vars. Auto-detected format.
- **Config DB resilience**: If the encrypted config DB can't be opened (wrong password, corruption), creates a fresh database instead of crashing.
- **`config_sync_bucket` in TOML**: Config DB S3 sync bucket now configurable via TOML (was env-only).

### UI

- **Favicon**: Teal chain-link icon on dark background.
- **Reduced polling**: Browser file listing polls every 30s instead of every second.

### Defaults

- **`max_delta_ratio`**: Default raised from 0.5 to 0.75 (more files stored as deltas, better space savings for typical workloads).

### Code Quality

- Removed dead `http_put` test helper from storage resilience tests.

## v0.5.3

### Performance

- **Parallel delta reconstruction**: Reference and delta fetched concurrently via `tokio::join!` instead of sequentially. Saves ~100ms per delta GET on S3 backends.
- **Parallel metadata HEAD calls**: `resolve_object_metadata` fetches delta and passthrough metadata concurrently.
- **Legacy migration off GET path**: `migrate_legacy_reference_object_if_needed` no longer runs during GET (was adding 60+ seconds of xdelta3 encoding). Available via batch migration endpoint instead.

### Correctness

- **PATHOLOGICAL warnings**: Delta and reference files missing DG metadata now log prominent warnings instead of silently falling back.
- **`dg-ref-key` → `dg-ref-path` rename**: Reference paths stored as relative paths for portable deltaspaces. Legacy `dg-ref-key` read with automatic fallback.

### Testing

- **Metadata validation tests**: 7 new tests for missing, corrupt, partial, and wrong metadata scenarios.

### UI

- **Connect page simplification**: Shows only relevant fields per auth mode (bootstrap password OR S3 credentials, not both). Removed endpoint URL field (derived from window location).

## v0.5.2

### Correctness (Critical)

- **Fix GET on rclone-copied delta files**: `retrieve_stream` used `obj_key.filename` instead of `metadata.original_name` for storage lookups, causing 404 on files whose S3 key had a `.delta` suffix. This bug was not caught by 222 tests because they all follow the clean PUT→GET path.

### Testing

- **Storage resilience tests**: 6 new adversarial tests that would have caught the above bug:
  - Triangle invariant (LIST→HEAD→GET must all succeed for every key)
  - HEAD/GET content-length consistency
  - SHA256 roundtrip verification across all storage strategies
  - Unmanaged file triangle invariant (directly placed files)
  - External delete → 404 (not stale cached data)
  - LIST never exposes `.delta` suffix or `reference.bin`

### Security

- **Session cookie hardening**: Secure, HttpOnly, SameSite=Strict attributes.
- **Remove secrets from whoami**: `/whoami` endpoint no longer exposes secret access keys.
- **Cleanup `.bak` files**: Removed leftover backup files from refactoring.

## v0.5.1

### Security Fixes (Post-Release Audit)

- **Deny condition bypass**: Fixed condition evaluation that could skip Deny rules under certain group membership combinations.
- **Group `member_ids` persistence**: Group member lists were not persisted correctly to SQLCipher DB.
- **`evaluate_iam` alternate path**: Fixed edge case where IAM evaluation took a non-standard code path.
- **Session storage**: Server-side session storage for S3 credentials (no longer stored client-side).
- **`env_clear()` on subprocess**: xdelta3 subprocess environment cleared to prevent credential leaks.
- **`DGP_TRUST_PROXY_HEADERS`**: Default changed to `true` for reverse proxy deployments.

### Testing

- **Auth integration tests**: SigV4 signature verification, presigned URLs, clock skew rejection, IAM lifecycle.
- **IAM persona tests**: 23 tests for groups, ListBuckets filtering, prefix scoping, conditions.

### Infrastructure

- **Docker retry**: `apt-get` retries on network blips (`Acquire::Retries=3`).
- **Startup refactor**: Extracted `startup.rs` from `main.rs`, split `engine.rs` and `config_db.rs` into sub-modules.

## v0.5.0

### IAM Policy Conditions (iam-rs)

- **Full AWS IAM condition support**: Integrated `iam-rs` crate for standards-compliant policy evaluation with all AWS condition operators (StringEquals, StringLike, IpAddress, NumericLessThan, etc.).
- **`s3:prefix` condition**: Deny LIST requests based on the prefix query parameter. Example: `{"StringLike": {"s3:prefix": ".*"}}` blocks listing dotfiles.
- **`aws:SourceIp` condition**: Restrict operations to specific IP ranges. Example: `{"IpAddress": {"aws:SourceIp": "10.0.0.0/8"}}`.
- **DB schema v4**: New `conditions_json` column on permissions and group_permissions tables. Backward compatible — existing permissions work unchanged.
- **Permission validation**: Effect normalization (case-insensitive), max 100 rules per user/group, resource pattern validation (trailing wildcard only), `$`-prefix names blocked.
- **Frontend conditions UI**: Collapsible conditions section per permission rule with prefix and IP restriction inputs.

### IAM Module Refactor

- **Split `iam.rs` into module**: `iam/types.rs`, `iam/permissions.rs`, `iam/middleware.rs`, `iam/keygen.rs`, `iam/mod.rs`. Pure permission evaluation logic separated from framework-specific middleware.
- **Centralized permission authority**: All permission checks go through `AuthenticatedUser.can()`, `.can_with_context()`, `.can_see_bucket()`, `.is_admin()`.
- **Legacy admin as AuthenticatedUser**: Bootstrap credentials now create a `$bootstrap` AuthenticatedUser with wildcard permissions instead of bypassing authorization entirely.
- **Groups loaded on startup**: Previously groups were only loaded on first IAM mutation.
- **ListBuckets per-user filtering**: Users see only buckets they have permissions on.
- **IAM backup/restore**: Export/import all users, groups, permissions, and credentials as JSON via `/_/api/admin/backup`.

### Security Fixes

- **Batch delete per-key authorization**: Previously the middleware only checked delete permission at bucket level; now each key in a batch delete is individually authorized.
- **Progressive auth delay**: Failed auth attempts trigger exponential backoff (100ms→5s), making brute force expensive before lockout.
- **Attack detection logging**: `SECURITY |` log events for brute force detection, lockout, and repeated failures with IP and attempt count.
- **Condition parse fail-closed**: Malformed conditions produce an empty policy (deny-all) instead of stripping conditions (fail-open).
- **Error XML escaping**: Error `<Message>` now XML-escaped to prevent malformed responses.
- **Bucket names with dots**: S3-compliant validation now accepts dots in bucket names.
- **is_admin strict check**: Uses `== "Allow"` instead of `!= "Deny"`.

### Performance

- **Range passthrough**: Range requests on passthrough objects pass the Range header through to upstream S3 (or seek on filesystem) instead of buffering the entire file. A 1KB range on a 100MB file reads only 1KB from storage.
- **Request concurrency limit**: `tower::limit::ConcurrencyLimitLayer` with configurable max (default 1024, `DGP_MAX_CONCURRENT_REQUESTS`).
- **Write-before-delete**: Storage strategy transitions (delta↔passthrough) now write the new variant before deleting the old one, preventing transient 404s on concurrent GETs.
- **Prefix lock cleanup**: `cleanup_prefix_locks()` runs on every lock acquisition instead of only on delete.
- **Interleave-and-paginate dedup**: Shared function for the interleave/sort/paginate pattern used by engine, S3 backend, and filesystem backend.

### Unified Audit Logging

- **`src/audit.rs`**: Single audit module with `sanitize()`, `extract_client_info()`, and `audit_log()`. Eliminates duplicated sanitization and IP extraction across handlers and admin API.
- **Session TTL decoupled**: `SessionStore::ttl()` method replaces re-parsing `DGP_SESSION_TTL_HOURS` in the cookie formatter.

### Frontend

- **Conditions UI**: Permission editor supports prefix restriction and IP restriction inputs with collapsible conditions panel per rule.
- **IAM backup buttons**: Export/Import buttons in admin sidebar for JSON backup/restore.
- **Log level radio buttons**: Replaced broken Select dropdown with Radio.Group.
- **Polling fix**: Users/Groups panels load once on mount instead of re-polling on every render.
- **Deduplicated formatters**: Unified byte formatters, extracted CredentialsBanner and InspectorSection components.

### Tests

- **222 tests**: 180 unit + 42 integration (23 new persona tests covering groups, ListBuckets filtering, prefix scoping, cross-user isolation, multipart, content verification, deny-from-groups, IAM conditions).
- **iam-rs condition tests**: Unit tests for prefix deny with StringLike, IP deny with IpAddress.

### Metadata Cache

- **In-memory metadata cache**: New moka-based cache (`MetadataCache` in `metadata_cache.rs`) eliminates redundant HEAD calls for object metadata. 50 MB default budget (~125K–150K entries), 10-minute TTL. Populated on PUT, HEAD, and LIST+metadata=true. Consulted on HEAD, GET, and LIST (including for file_size correction on delta-compressed objects). Invalidated on DELETE and prefix delete. Configurable via `DGP_METADATA_CACHE_MB` env var or `metadata_cache_mb` TOML setting.

### Security Hardening

#### Tier 1 — Authentication & Session Security
- **Rate limiting**: Per-IP token bucket rate limiter on auth endpoints — 5 attempts per 15-minute window, 30-minute lockout after exhaustion. Prevents brute-force attacks on admin login.
- **Session IP binding**: Admin sessions are bound to the originating IP address. Requests from a different IP are rejected even with a valid session token.
- **Session concurrency cap**: Maximum 10 concurrent admin sessions. Oldest session evicted when the limit is reached.
- **Configurable session TTL**: Default reduced from 24h to 4h. Override with `DGP_SESSION_TTL_HOURS`.
- **Password quality enforcement**: Min 12 chars, max 128 chars, common password blocklist. Validated on both admin API and CLI password set flows.
- **SigV4 replay detection**: Duplicate signatures within a 5-second window are rejected to prevent request replay attacks.
- **Presigned URL max expiry**: Capped at 7 days (604,800 seconds), matching AWS S3.
- **Configurable clock skew**: `DGP_CLOCK_SKEW_SECONDS` (default 300s) controls SigV4 timestamp tolerance.

#### Tier 2 — Response Hardening & Anti-Fingerprinting
- **Security response headers**: All responses include `X-Content-Type-Options: nosniff` and `X-Frame-Options: DENY`. HSTS header added when TLS is enabled.
- **Anti-fingerprinting**: Debug/fingerprinting headers (`Server`, `x-amz-storage-type`, `x-deltaglider-cache`) suppressed by default. Enable with `DGP_DEBUG_HEADERS=true`.
- **Bootstrap password TTY safety**: Auto-generated bootstrap password displayed in plaintext only when stderr is a TTY. Hidden in container/CI/piped output to prevent credential leaks in log aggregators.
- **Multipart upload limits**: Concurrent multipart uploads capped at 100 (configurable via `DGP_MAX_MULTIPART_UPLOADS`) to prevent resource exhaustion.

### Usage Scanner

- **Background prefix size scanner**: `/_/api/admin/usage` endpoint computes prefix sizes asynchronously with 5-minute cached results, 1,000-entry LRU cache, and 100K-object scan cap per prefix.

## v0.4.0

### Single-Port Architecture

- **UI served at `/_/`**: The embedded admin UI and all admin APIs are now served under `/_/` on the same port as the S3 API. No more separate port (was port+1). The `/_/` prefix is safe because `_` is not a valid S3 bucket name character. Health, stats, and metrics endpoints are available at both root (`/health`) and under `/_/` (`/_/health`).

### IAM & Authentication

- **Bootstrap password**: Renamed from "admin password". A single infrastructure secret that encrypts the SQLCipher config DB, signs admin session cookies, and gates admin GUI access in bootstrap mode. Auto-generated on first run. Backward-compatible aliases (`DGP_ADMIN_PASSWORD_HASH`, `--set-admin-password`) still work.
- **Multi-user IAM (ABAC)**: Per-user credentials stored in encrypted SQLCipher database (`deltaglider_config.db`). Each user has access key, secret key, and permission rules with actions (`read`, `write`, `delete`, `list`, `admin`, `*`) and resource patterns (`bucket/*`). Admin = wildcard actions AND wildcard resources.
- **IAM mode auto-activation**: When the first IAM user is created, the proxy switches from bootstrap mode to IAM mode. Bootstrap credentials are migrated as "legacy-admin" user. Admin GUI access becomes permission-based (no password needed for IAM admins).
- **Admin API**: `/_/api/admin/users` CRUD, `/_/api/admin/users/:id/rotate-keys`, `/_/whoami`, `/_/api/admin/login-as` for IAM user impersonation.

### S3 API Compatibility

- **Range requests**: `Range` header support with 206 Partial Content responses, `Accept-Ranges: bytes` header on all object responses.
- **Conditional headers**: `If-Match` / `If-Unmodified-Since` (412 Precondition Failed), `If-None-Match` / `If-Modified-Since` (304 Not Modified).
- **Content-MD5 validation**: Validates `Content-MD5` header on PUT and UploadPart, rejects with 400 on mismatch.
- **Copy metadata directive**: `x-amz-metadata-directive: COPY` (default) or `REPLACE` on CopyObject.
- **ACL stubs**: GET/PUT `?acl` accepted and ignored for SDK compatibility.
- **Response header overrides**: `response-content-type`, `response-content-disposition`, `response-content-encoding`, `response-content-language`, `response-expires` query parameters on GET.
- **Per-request UUIDs**: `x-amz-request-id` header with unique UUID on every response.
- **Bucket naming validation**: Extracted `ValidatedBucket` and `ValidatedPath` extractors for automatic S3 path validation.
- **ListObjectsV2 improvements**: `start-after` parameter, `encoding-type` passthrough, `fetch-owner` support, base64 continuation tokens, max-keys capped at 1000.
- **Real creation dates**: `ListBuckets` returns actual bucket creation timestamps.

### Performance

- **Lite LIST optimization**: LIST operations no longer issue per-object HEAD calls. Sizes shown are stored (compressed) sizes. ~8x faster for large listings.
- **FS delimiter optimization**: `list_objects_delegated()` for filesystem backend uses a single `read_dir` at the prefix directory instead of a recursive walk when a delimiter is specified. Dramatically faster for buckets with many prefixes.

### Security

- **OsRng for tokens**: Session tokens and IAM access keys use `OsRng` (cryptographically secure) instead of `thread_rng`.
- **DB rekey verification**: Bootstrap password changes verify the new key can open the database before committing.
- **Proper transactions**: IAM user creation uses database transactions for atomicity.

### Code Quality

- **`S3Op` enum** (`storage/s3.rs`): Operation context for S3 error classification, replacing string-based operation names.
- **Session cookie helpers** (`session.rs`): Extracted session store into its own module with `OsRng` token generation.
- **`env_parse()` DRY** (`config.rs`): Extracted environment variable parsing boilerplate into reusable helpers.
- **Dead code cleanup**: Removed `AdminGate`, unused `#/settings` route, dead `UsersTab` and `UserModal` components.

### UI Features

- **File preview**: Double-click on any previewable file (text, images) to view inline via the inspector panel. Tooltip indicates previewable files.
- **Show/hide system files**: Toggle to show or hide DeltaGlider internal files (`.dg/` directory contents) in the object browser.
- **Folder size computation**: Folder sizes computed and displayed in the object table.
- **Delete user confirmation**: User deletion requires `window.confirm` dialog.
- **Full-screen admin overlay**: Admin settings now use a full-screen overlay with master-detail layout for user management.
- **Interactive API reference**: New `#/docs` page with interactive API documentation.
- **Key rotation safety**: Prevents self-lockout on key rotation; changing only the secret key no longer regenerates the access key.
- **Credentials display**: After creating a user, shows only the credentials with a dismissible banner before returning to the user list.

## v0.3.0

### S3-Compatible Endpoint Support

- **Disabled automatic request/response checksums**: AWS SDK for Rust (like boto3 1.36+) adds CRC32/CRC64 checksum headers by default. S3-compatible stores (Hetzner Object Storage, Backblaze B2, some MinIO configs) reject these with BadRequest. Now sets both `request_checksum_calculation` and `response_checksum_validation` to `WhenRequired`. (Port of Python DeltaGlider [6.1.1] fix.)
- **Retry with exponential backoff on PUT**: Hetzner returns transient 400 BadRequest errors (~1-2% of requests) with `connection: close` and no request-id. PUT operations now retry on 400 and 503 with 100/200/400ms backoff (3 retries). Also retries on network/timeout errors.
- **Verbose S3 error logging**: Every S3 error now logs operation, bucket, HTTP status code, `x-amz-request-id`, and full error details for production debugging.

### Unmanaged Object Support (Fixes #3, #4)

Objects that exist on the backend storage but were never stored through the proxy (no DeltaGlider metadata) are now fully accessible:

- **S3 backend**: HEAD, GET, and LIST now return fallback metadata from the S3 HEAD response (size, ETag, Last-Modified) instead of 404
- **Filesystem backend**: HEAD, GET, and LIST now return fallback metadata from filesystem stats (size, mtime) instead of 404
- **HEAD/GET consistency**: Both operations return metadata from the same source, ensuring consistent Content-Length and ETag
- **Delta and reference files**: Fallback metadata also works for delta/reference files without metadata (xattr or S3 headers)
- **Corrupt metadata recovery**: S3 objects with partial/corrupt DG headers now fall back to passthrough instead of hard-failing

### Error Handling Hardening

- **Error discrimination**: Replaced blanket `.ok()` and `map_err(|_| NotFound)` patterns with explicit error matching throughout the engine. `NotFound` → expected (object doesn't exist), `Io` → warn + retry path (concurrent access), other errors → propagate as 500
- **ENOENT classification**: `io_to_storage_error()` now maps file-not-found I/O errors to `StorageError::NotFound` instead of `StorageError::Io`, preventing false 500s for missing files
- **Filesystem xattr errors**: Only fall back to filesystem stats on `NotFound`; permission denied and other I/O errors are now propagated instead of silently swallowed
- **Reference cache errors**: `get_reference_cached()` now discriminates `NotFound` (→ MissingReference) from other storage errors (→ Storage)

### Security

- **SigV4 clock skew validation**: Regular (non-presigned) SigV4 requests now enforce a 15-minute clock skew window, matching AWS S3 behavior. Prevents replay attacks with arbitrarily old timestamps. Returns new `RequestTimeTooSkewed` error (403).
- **Reserved filename validation**: PUT requests for `reference.bin` and `*.delta` keys are rejected with 400 to prevent collision with internal storage files

### Reliability

- **Codec subprocess timeout**: xdelta3 subprocess now has a 5-minute timeout via `try_wait` polling loop. Kills hung processes and returns an error instead of blocking indefinitely.
- **Copy object size check**: `copy_object` now verifies actual data size after retrieval, catching cases where fallback metadata reports `file_size=0` that would bypass the pre-copy size check
- **S3 metadata size validation**: Rejects PUT if DeltaGlider metadata headers exceed S3's 2KB limit, instead of letting the upstream S3 return an opaque 400
- **Config validation**: Warns on `max_delta_ratio` outside [0.0, 1.0] and `max_object_size=0` at startup
- **Cache invalidation ordering**: Reference is now deleted from storage BEFORE cache invalidation, preventing concurrent GET from re-caching a stale reference between invalidation and deletion. Fixed in 3 code paths (passthrough fallback, deltaspace cleanup, legacy migration).

### DRY Cleanup

- **`FileMetadata::fallback()`**: New constructor consolidates 4 duplicate fallback metadata construction sites across S3 and filesystem backends
- **`Engine::validated_key()`**: Extracts the 5x repeated `ObjectKey::parse + validate_object + deltaspace_id` pattern
- **`try_unmanaged_passthrough()`**: Extracted 60-line nested match block from `retrieve_stream()` into a focused helper with flat control flow

### Testing

- 26 new tests (244 total, up from 218): unmanaged object operations, HEAD/GET metadata consistency, reserved filename rejection, copy with unmanaged sources, error discrimination (`io_to_storage_error` unit tests), delta byte-level round-trip integrity, multipart ETag format, user metadata round-trip, external file deletion → 404

### Infrastructure

- Docker build: native ARM64 on Blacksmith runners (no QEMU), cargo-chef for dep caching — ~5x faster builds
- RustSec: updated deps to fix 6 advisories in aws-lc-sys and rustls-webpki

## v0.2.0

### Cache Health Observability

Four layers of defense against silent cache degradation:

- **Startup warnings**: `[cache]` log prefix warns when cache is disabled (0 MB) or undersized (<1024 MB)
- **Periodic monitor**: Every 60s, warns on >90% utilization or >50% miss rate
- **Prometheus metrics**: `cache_max_bytes`, `cache_utilization_ratio`, `cache_miss_rate_ratio` — computed on scrape from existing atomic counters
- **Response header**: `x-deltaglider-cache: hit|miss` on delta-reconstructed GETs
- **Health endpoint**: `/health` now includes `cache_size_bytes`, `cache_max_bytes`, `cache_entries`, `cache_utilization_pct`

### Proxy Dashboard

Full Prometheus metrics dashboard in the built-in React UI (`#/metrics`):

- Top KPIs: uptime, peak memory, total requests, storage savings %
- Cache section: utilization gauge, hit rate with color coding, live hits vs misses chart
- Delta compression: encode/decode latency, compression ratio distribution, storage decisions
- HTTP traffic: operation breakdown (bar + donut chart), latency distribution, status codes, live request rate
- Authentication: success/failure counts with failure reason breakdown
- Auto-refresh every 5s, storage stats every 60s

### Correctness Fixes (11 bugs)

- **Codec stderr deadlock**: xdelta3 stderr was piped but only drained after stdout. If xdelta3 writes >64KB to stderr, stdout reader blocks forever. Now drains all 3 pipes concurrently.
- **Filesystem metadata-data split-brain**: Crash between `atomic_write` and `xattr_meta::write_metadata` left files without metadata. Now writes xattr to temp file before rename — atomic visibility.
- **Auth exemption path normalization**: `/health/` (trailing slash) was not matched by the exact-string exemption. Now strips trailing slashes.
- **SigV4 presigned URL case-sensitivity**: `x-amz-signature` (lowercase) was not excluded from canonical query string. Now uses case-insensitive comparison.
- **Stats cache thundering herd**: Lock released before `compute_stats`, so N concurrent requests all scanned storage. Now holds `tokio::sync::Mutex` across the async compute.
- **Multipart double-completion race**: Two concurrent `CompleteMultipartUpload` calls both read parts under read lock, both stored data. Now takes ownership under write lock atomically.
- **Multipart write lock starvation**: Assembly of large uploads held write lock during memcpy of all parts. Now removes upload from map (fast), releases lock, then assembles.
- **AWS chunked truncated payload**: Decoded length mismatch with `x-amz-decoded-content-length` was logged but data stored anyway. Now rejects with 400.
- **Admin config rollback**: Backend config was committed before engine swap. If `DynEngine::new()` failed, config showed new backend but engine was old. Now rolls back on failure.
- **S3 403 misclassification**: All 403 errors mapped to `BucketNotFound`. Now only maps for bucket-level operations; object-level 403 is reported as S3 error.
- **list_objects max_keys=0**: Produced `is_truncated=true` with no continuation token. Now clamps to >= 1.

### DRY & Code Quality

- **`StoreContext` parameter object**: Eliminated 3 `#[allow(clippy::too_many_arguments)]` suppressions from `encode_and_store`, `store_passthrough`, `set_reference_baseline`
- **`with_metrics()` helper**: Collapsed 8 inline `if let Some(m) = &self.metrics` blocks to one-liners
- **`try_acquire_codec()`**: Extracted codec semaphore acquisition with consistent error message
- **`cache_key()`**: Extracted `format!("{}/{}", bucket, deltaspace_id)` used 5 times
- **`delete_delta_idempotent()` / `delete_passthrough_idempotent()`**: Extracted 5 verbose `delete_ignoring_not_found` call sites
- **`paginate_sorted()`**: Extracted duplicated pagination logic from `list_objects`
- **`pipe_stdin_stdout_stderr()`**: Extracted duplicated codec pipe coordination from encode/decode
- **`object_url()`**: Extracted test helper URL construction
- **`parse_env()` / `parse_env_opt()`**: Extracted config env var parsing boilerplate
- **`header_value()`**: Renamed from cryptic `hval()`
- **`_guard`**: Renamed from misleading `_lock` (the Drop impl is load-bearing)
- Trimmed verbose `PERF:` archaeology comments — kept constraints, removed history
- Removed `Option<Arc<Metrics>>` from `AppState` — metrics are always present, eliminated branch per request
- Replaced `format!("%{:02X}")` with `write!()` in SigV4 URI encoding — no heap allocation per encoded byte

### Infrastructure

- `/stats` endpoint: 10s server-side cache, capped at 1,000 objects with `truncated` indicator
- `/health`, `/stats`, `/metrics` exempted from SigV4 authentication
- Demo server exposes `/health`, `/stats`, `/metrics` alongside admin API
- `prefixed_key()` helper in S3 backend eliminates 3x prefix if/else duplication

## v0.1.9

Initial public release with S3-compatible proxy, delta compression, filesystem and S3 backends, SigV4 authentication, multipart uploads, embedded React UI, and Prometheus metrics.
