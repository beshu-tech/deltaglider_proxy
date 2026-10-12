# Node-local tables leave the synced config DB (plan P4.4)

Status: decided, Option A with a compatibility shape (empty tables stay in the
config DB for now). Written before the code, as P4.4 asks.

## 1. The problem

`deltaglider_config.db` is one SQLCipher file. With `config_sync_bucket` set,
the whole file is uploaded after every IAM mutation and downloaded by every
peer on each change (`config_db_sync.rs`: `read_db_snapshot` reads the file,
`upload` PUTs it, `download_if_newer` GETs it, `write_sync_base` keeps a full
copy as `<db>.sync-base`). The file also holds the job plane of the node: the
event outbox (rows live 24 h), the replication parity cache (one row per
object, per side, per rule), run history, failure rings, leases and
maintenance jobs. Commit bae52e25 cites a 338 MB prod file. An IAM change of
one row uploads all of it, every peer downloads all of it, and every node
keeps one more full copy as the merge base.

## 2. Every table in the config DB

Source: `ConfigDb::migrate_steps` (`src/config_db/mod.rs`, v1-v29) and the
schema helpers in `src/config_db/iam_merge.rs` (`install_mtime_schema`,
`install_rule_uid_schema`, `install_generation_schema`). No other module
creates a table in this file (`grep "CREATE TABLE" src`: the other hits are
`bucket_usage.rs`, which is its own file, and test fixtures).

### Synced (IAM): stay in the config DB

| Table | Evidence |
|---|---|
| `users`, `permissions` | `iam_merge::SPECS` (users, child permissions); read by `rebuild_iam_index`. |
| `groups`, `group_permissions`, `group_members` | `SPECS` (groups + child permissions, `group_members`). |
| `auth_providers` | `SPECS`. |
| `group_mapping_rules` | `SPECS` (by `rule_uid`, v28). |
| `external_identities` | `SPECS`; restored by `config_db/restore.rs`. |
| `session_revocations` | `merge_iam_from` MAX-upsert from `remote.session_revocations`. |
| `sync_state` | `sync_generation` (v29): `read_generation`, `next_sync_generation`, rollback check. |
| `sqlite_sequence` | SQLite internal (AUTOINCREMENT). |

The merge reads ONLY these: `read_snapshot` walks `SPECS`, and the only other
statements on `remote.*` are the `session_revocations` upsert and the
`sync_state` generation read. `restore.rs` (backup import) writes only IAM
tables. `iam_row_count` counts only IAM tables.

### Node-local: move out

| Table | Readers / writers | Merge |
|---|---|---|
| `replication_state` | `replication/state_store.rs`, worker, walk, admin `jobs.rs` | not touched |
| `replication_run_history` | state_store, worker, admin `jobs.rs` | not touched |
| `replication_failures` | state_store | not touched |
| `replication_object_failures` | state_store (poison-object ledger) | not touched |
| `replication_parity_objects` | state_store, `replication/parity.rs` (the big one) | not touched |
| `replication_parity` | state_store (audit verdict + lease) | not touched |
| `event_outbox` | `event_outbox.rs` (appender, prune), `event_delivery.rs`, `replication/event_consumer.rs`, admin `event_outbox.rs` | not touched |
| `event_deliveries` | `event_outbox.rs`, `event_delivery.rs` (FK to `event_outbox`) | not touched |
| `listener_cursors` | `event_outbox.rs`, `replication/event_consumer.rs` | not touched |
| `lifecycle_state`, `lifecycle_run_history`, `lifecycle_failures` | `lifecycle/state_store.rs`, worker, scheduler | not touched |
| `maintenance_jobs`, `maintenance_failures` | `maintenance/store.rs`, worker, gate, migrate, backfill | not touched |

Leases (`leader_instance_id`/`leader_expires_at` columns, `config_db/job_store.rs`)
live in these tables; they are node-local since B3, and they move with them.
`LocalLease` (`coordination/lease.rs`) reaches them through the same handle.

All SQL on these tables sits in `impl ConfigDb` blocks in four files
(`event_outbox.rs`, `replication/state_store.rs`, `lifecycle/state_store.rs`,
`maintenance/store.rs`) plus a few raw statements in `maintenance/gate.rs`,
`maintenance/worker.rs`, `replication/parity.rs` and `lifecycle/worker.rs`. No
function mixes an IAM table and a node-local table in one statement or
transaction (checked: `reconcile_job_rules` in `config_apply.rs` touches only
job tables; the boot reconciles in `startup.rs` call only job methods).

## 3. Options

**Option A: a second, unsynced SQLCipher file** `deltaglider_jobs.db`, next to
the config DB, same key. The precedent is `deltaglider_usage.db`
(`bucket_usage.rs`), which already moved its counters out.

**Option B: upload a filtered snapshot** (copy the file, drop node-local
tables, VACUUM, upload). Every upload then costs a full copy plus a VACUUM of
the live file; the sync base and the uploaded bytes differ from the local
file (today `write_sync_base` writes exactly the uploaded bytes, and a merge
renames the download into the base; both stay true, but the local file and
the base drift in shape); the 338 MB file still sits on disk, still slows
`read_db_snapshot`, and the job writes still move the change token that the
upload snapshot retries on. B only shrinks the network leg.

A also gives the job plane its own connection and its own lock: today an
outbox append and an IAM auth read wait on one `tokio::sync::Mutex<ConfigDb>`.

**Decision: A.**

## 4. Design of A

### 4.1 The type

`JobsDb` (`src/config_db/jobs_db.rs`) owns its own `Connection` and is shared
as `Arc<tokio::sync::Mutex<JobsDb>>`. The four `impl ConfigDb` blocks become
`impl JobsDb`. `ConfigDb` loses every node-local method, so a call site that
holds a `ConfigDb` cannot reach a job table through the API: the compiler
moves every call site. `JobPlane.db`, the replication/lifecycle/maintenance
workers, the outbox appender, the dispatcher, the event consumer and
`LocalLease` take `JobsDb`. A source guard refuses raw SQL on a node-local
table name in any file that has an `impl ConfigDb` block, so the
`pub(crate) conn` of `ConfigDb` cannot be used to write a job row by hand.

### 4.2 Schema of the jobs DB

The jobs DB starts at its own `user_version` 1 with the final shape of the
14 tables (columns in the same order as in the config DB, the same indexes,
the same FKs: `event_deliveries -> event_outbox`, history/failures ->
state). A unit test compares `PRAGMA table_info` and the index list of each
table in a fresh config DB and a fresh jobs DB, so the two cannot drift
before the copy. New node-local columns go into jobs DB migrations from now
on. `auto_vacuum = INCREMENTAL` from the start; `reclaim_free_pages` runs on
the jobs connection after prunes, as today. Journal mode stays the rollback
journal with the default `synchronous` (no behaviour change; WAL is P4.5).

### 4.3 The empty tables stay in the config DB (compatibility shape)

The move deletes the ROWS from the config DB, not the tables, and the config
DB schema version stays 29. Why:

- **Old peer downloads a new blob.** An old binary opens the copy
  (`open_with_keys` -> `migrate`: v29 = up to date, nothing runs) and merges
  only IAM tables (section 2). A copy with empty node-local tables is a valid
  v29 DB for it. Measured cost of the empty tables: about one page per table
  and index (about 100 KB).
- **No schema bump.** A bump to v30 makes every old peer refuse every new
  blob (`download_if_newer`: "newer than this binary ... NOT merging"), so
  IAM sync between old and new nodes stops for the whole rolling upgrade.
  Without a bump, old and new nodes keep merging IAM both ways.
- **Downgrade.** An old binary opens a config DB that still has every table;
  the tables are empty, so it loses the job history and the cursors, not IAM,
  and it does not crash on "no such table".

(Dropping the tables would also be safe for old peers' merges, because the
old merge never reads them, but it breaks a downgrade. A later release can
drop them with a schema bump once no pre-split peer is left.)

New code never writes these tables: `ConfigDb` has no method for them and
the source guard in 4.1 refuses raw SQL.

### 4.4 The move (boot only, live DB only)

It runs in `startup::init_config_db` after the live config DB opened with no
key mismatch, before any job subsystem, the sync, or a request starts. It
NEVER runs inside `ConfigDb::open_or_create`: that open also runs on
downloaded copies (`.db.tmp.*`), on the merge base, on `.db.bak` probes and
on recovery temps, and none of those may feed the jobs DB.

The jobs DB has a one-row `jobs_meta` table with `adopted` (0/1).

1. Open (or create) the jobs DB with the config DB keys (4.6).
2. When `adopted = 0`: on the jobs connection, `ATTACH` the live config DB
   (read only by this step) and, in ONE transaction on the jobs DB:
   delete every row of the 14 tables in the jobs DB (they are not
   authoritative before adoption), copy every row of each table that exists
   in the config DB (columns matched by name: the intersection of both
   `table_info` lists; parents before children for the FKs), raise
   `sqlite_sequence` of each AUTOINCREMENT table to the config DB's value
   (MAX of both), and set `adopted = 1`. Commit. The sequence matters: the
   outbox ids must keep growing past every id a listener cursor has seen;
   a pruned outbox with a cursor at 1000 and a restart of ids at 1 would make
   the event consumer skip every new event.
3. On the config DB connection, in one transaction: delete every row of the
   14 tables (children first). Commit.
4. `VACUUM` the config DB once (best effort; the live data left is small, so
   it is fast; a failure leaves free pages that incremental vacuum returns
   later).

Each boot re-runs the check: `adopted = 1` and a node-local table in the
config DB that has rows -> step 3 and 4 again, with a warning that names the
counts. That is the state after a crash between step 2 and 3, or after an
old binary wrote job rows during a downgrade (section 6).

**Crash safety.** IAM rows are never written by the move. Step 2 is one
transaction in the jobs DB: a crash before its commit leaves `adopted = 0`
and the rows still in the config DB, and the next boot does step 2 again
from scratch (it clears the jobs tables first, so a re-run is idempotent).
A crash after the commit and before step 3 commits leaves the rows in both
files with `adopted = 1`: the next boot sees `adopted = 1`, so the jobs DB
is the truth, and it only deletes the leftovers from the config DB. A crash
in the VACUUM leaves the config DB as it was (VACUUM is atomic). The order
(copy commit, then delete commit) never relies on SQLite's multi-file atomic
commit.

### 4.5 Sync, merge base, rollback detection

Unchanged code. After the move the uploaded bytes hold IAM rows and empty
node-local tables. A new node that downloads a blob from an old peer (with
job rows) merges only IAM, and the job rows of that copy are never read: the
move reads only the live DB path, never a download, and the merge never reads
node-local tables. The merge renames the download into the base, so a base
from an old peer carries the peer's job rows on disk until the next upload
replaces it; nothing reads them. `sync_generation` and the row-age rollback
rule read `sync_state` and IAM rows only. `read_db_snapshot`'s change token
no longer moves on job writes (they go to the other file), so the lock-free
snapshot read retries less.

### 4.6 Keys, rotation, recovery

The jobs DB uses the config DB keys (`ConfigDbKeys`): the same primary
(`DGP_CONFIG_DB_KEY` or the key file) and the same fallbacks. It opens
through the same keyed-open routine as the config DB (generalised over the
open function): primary first, then each fallback with `rekey_file` (copy,
rekey, verify, rename) and `heal_companions`. So a rotation with
`DGP_CONFIG_DB_KEY_PREVIOUS` moves both files on the same boot.

Mismatch: when no key opens the config DB, the boot parks it as `.db.bak`
and locks the S3 API; the job workers do not start in that state today. The
jobs DB is then NOT opened at all, so the file stays as it is under its old
key; after the operator restores the key (recovery wizard, restart) it opens
normally. The recovery wizard probes only the config DB: the jobs DB has the
same key, so one answer covers both. A jobs DB that no key opens while the
config DB opens (a file copied in from another install) is renamed to
`deltaglider_jobs.db.unreadable-<unix time>` (never deleted) and a fresh one
is created, with an error log: job state is node-local and can be rebuilt,
IAM is not touched.

`--set-bootstrap-password` (`migrate_legacy_config_db_key`) handles a
pre-S8 config DB keyed by the bootstrap hash. A jobs DB never had that key:
it is created after the config DB moved to the primary key.

### 4.7 Backup and restore

`GET /api/admin/backup` exports `config.yaml`, `iam.json` and `secrets.json`
(logical JSON built from IAM queries), not a DB file. `POST
/api/admin/backup` restores IAM rows through `config_db/restore.rs` (users,
groups, providers, mapping rules, external identities with id remapping).
Neither touches a node-local table, so neither changes. The how-to page
"back up and restore" lists the file-level snapshot: it gains
`deltaglider_jobs.db` as optional (job history only).

### 4.8 Code sites that reach a node-local table

All move to the `JobsDb` handle: `JobPlane.db`; `replication::{scheduler,
worker, event_consumer, parity, walk}`; `lifecycle::{scheduler, worker}`;
`maintenance::{worker, gate, migrate, paged, backfill}`; `event_outbox`
(appender, flush, cursor notes); `event_delivery`; `coordination::LocalLease`;
`config_apply::reconcile_job_rules` and the rule-delete paths; the admin
handlers for jobs, the outbox and maintenance; the boot reconciles in
`startup.rs`; the gate seeding and shutdown flush in `main.rs`;
`AppState`/`AdminState` gain `jobs_db`.

## 5. Tests

- Move on a config DB with rows in every one of the 14 tables: rows and
  sequences preserved in the jobs DB, config DB tables empty, file smaller.
- Crash between the copy commit and the delete (simulated by running step 2
  only), then a re-run: no duplicate, no loss, config DB cleared.
- Crash before the copy commit (a failing hook inside the transaction), then
  a re-run.
- Mixed version: a v29 blob with job rows (what origin/main uploads) merged
  by the new code: IAM merged, jobs DB unchanged, live config DB job tables
  still empty. Reverse: a new blob (empty job tables, and a variant with the
  tables dropped) merged by the merge routine, which this change does not
  modify.
- Backup export + restore round trip with external identities still passes
  (unchanged code; existing tests rerun).
- Rotation: both files open with `DGP_CONFIG_DB_KEY_PREVIOUS` and end on the
  new key.
- Schema parity test (4.2) and the source guard (4.1).
- Size: a config DB seeded with 10k parity rows and 10k outbox rows, before
  and after the move.

## 6. Downgrade

After the move, an older binary starts on a config DB with empty job tables:
IAM is intact; job history, failure rings, parity cache, outbox rows,
replication and lifecycle cursors are gone for it (replication restarts its
walk from the start: copies are idempotent, so this is waste, not damage).
Finish or cancel maintenance jobs (migrate, reencrypt, backfill) before a
downgrade: the old binary does not see a running job, so it does not resume
it. Upgrading again restores the jobs DB as it was before the downgrade; job
rows that the old binary wrote into the config DB are deleted at that boot
(logged with counts), because their ids collide with the jobs DB's.

## 7. Not done here

- Dropping the empty tables from the config DB (needs a schema bump; after
  every peer runs a release with the split).
- WAL for the jobs DB (P4.5 question; no measurement yet).
