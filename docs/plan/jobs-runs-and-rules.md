# Jobs: runs on one page, rules on their own pages

Status: in progress (2026-10-16). Phase 1 done. Owner: Simone + Claude.

## Problem

The Jobs page mixes two kinds of row in one table:

- A **rule** (replication, lifecycle) is a definition in the YAML config. It lives
  until someone deletes it and runs many times. Its row shows only its latest run.
- A **one-off** (delete, re-encrypt, migrate, backfill) is one execution in the jobs
  DB. It runs once and is then history.

Effects: the "Trigger" column is the only clue to which row is which; a finished
one-off had no actions; 50 bulk deletes bury the two rules; one dirty bar drives two
YAML editors through a sequential apply queue; the lifecycle scheduler switch sits on
top of a list of one-offs. Rule run history has no retention: an hourly lifecycle
rule adds about 8,760 rows a year.

## Model

A one-off is a run without a rule. Every run has the same shape: origin (rule +
trigger, or the person and surface that started it), scope, status, progress,
counts, failures. So:

- **Runs belong together** on one page: Jobs.
- **Rule definitions get their own pages**: Replication, Lifecycle.

## Navigation

Storage group: Backends · Buckets · **Replication** · **Lifecycle** · **Jobs**.

| Page | Path | Holds |
|---|---|---|
| Replication | `storage/replication` | replication rules, `storage.replication` settings, Verify, Clone |
| Lifecycle | `storage/lifecycle` | lifecycle rules, scheduler switch + interval, Preview, Clone |
| Jobs | `jobs` | every run: rule runs and one-offs, live and past; New one-off |

Rules do not move under Buckets: one replication rule spans two buckets, and both
rule lists are global YAML sections with global settings.

## Phase 1 — backend: one runs API + retention

- `GET /_/api/admin/jobs/runs` merges replication runs, lifecycle runs and one-offs,
  newest first (by start time). Query: `status=active|failed|all`, `kind`,
  `rule=<kind>:<name>`, `bucket`, `noop` (default false: hide finished rule runs
  that changed nothing), `cursor`, `limit` (default 50, max 200).
- Each entry: `id` (`<job id>#<run id>` for a rule run, the job id for a one-off),
  `job_id`, `run_id`, `kind`, `rule` (rule runs), `origin` (`schedule` / `run-now` /
  `event` / `admin` / `file-browser` …), `started_by`, `scope`, `status`, counts,
  `percent` when known, timestamps, `errors`. One cursor across three tables
  (key: start time, kind rank, id).
- `GET /jobs/:id/failures?run_id=` filters a rule's failures to one run.
- Retention: per rule, drop runs older than 30 days that changed nothing, and keep at
  most the newest 1,000 runs. Finished one-offs older than 90 days are dropped.
  Pruned after each run ends and on the maintenance worker tick.

## Phase 2 — Jobs page (runs)

- Filter chips: Active (default when something runs, else All) · Failed · All; kind;
  bucket; "Show runs that changed nothing"; a rule chip when opened from a rule page.
- Columns: Job (kind tag + what), Origin, Scope, Status (progress bar while active),
  When, actions (Cancel / Kill while active; Remove for a finished one-off).
- A run drawer: summary counts, timing, origin, the run's failures; for a one-off its
  parameters and Cancel; for a rule run a link to its rule.
- "New one-off" menu: re-encrypt, migrate, backfill (bulk delete starts from the file
  browser). Live polling as today.

## Phase 3 — Replication and Lifecycle pages (definitions)

- Split today's `JobsPanel` into one rules page per kind. Each page owns ONE section
  editor and ONE dirty bar; the sequential apply queue goes away.
- Settings sit on the page: lifecycle switch + interval; replication switch + tick.
- Rule row: route/scope, trigger, health, last run, "N runs →" (Jobs filtered to the
  rule), actions (pause/resume, Run now, Verify/Preview, ⋯).
- **Clone** (row "⋯" and drawer "⋯"): copies the rule (saved or draft, with its unsaved
  edits) into a new draft `<name>-copy`, opens it, and the operator points it at
  other source / destination buckets before Apply. The drawer warns while another rule
  has the same scope (replication: source, destination and filters; lifecycle: bucket,
  prefix, action kind and filters).
- The rule drawer keeps Definition / Runs / Failures / Verify / Preview.

## Phase 4 — cut-over

- Old URLs remap: `jobs?job=replication:x` → `storage/replication?job=replication:x`
  (same for lifecycle); `configuration/storage/{replication,lifecycle}` → new pages.
  Command palette entries, browser banners.
- Docs: reference/jobs.md, replication.md, lifecycle.md, the how-to pages that say
  "Jobs → New job", screenshots, CHANGELOG. E2E specs that open the Jobs page.

## Phase 5 — polish

- Buckets page: the rules that read or write a bucket, linked.
- Delete runs show the folder and the person who started them.
