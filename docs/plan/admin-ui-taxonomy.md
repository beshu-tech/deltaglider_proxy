# Admin UI taxonomy study — minimal control system + architectural overlaps

*Status: SHIPPED (phases 1-6; the worker-loop orchestrator §4.3-4 is in progress as the final phase). Method: exhaustive code inventory of every admin screen/control
(19 leaf pages, per-field counts) + side-by-side architectural inventory of every
background subsystem (6 job-like + 2 gates). Companion to
`storage-ui-cognitive-load.md` (shipped), extending the same question — "make
people think less" — from two screens to the whole admin surface.*

---

## 1. The abstract taxonomy: what actually needs controlling

Strip away the implementation and an operator of this product controls exactly
**six domains**:

| # | Domain | The operator's question | Examples |
|---|--------|------------------------|----------|
| A | **Access** | *Who can do what?* | users, groups, permissions, external IdPs, admin password, pre-auth request gates (IP/method/anonymous) |
| B | **Storage topology** | *Where do bytes live?* | backends (path/endpoint/creds/encryption-at-rest), buckets (routing, exposure, quota, compression) |
| C | **Data jobs** | *What happens to data over time?* | continuous copy (replication), scheduled expiry/transition (lifecycle), one-off transforms (re-encrypt, migrate) |
| D | **Integrations** | *Who gets told?* | object events → webhook / Slack |
| E | **Platform** | *How does the process run?* | listener/TLS, caches, limits, logging, HA sync, backup/restore |
| F | **Observability** | *What is happening?* (read-only) | health/metrics, audit, trace, job runs/failures, outbox, efficiency scans |

Six domains. Everything in the product fits one of them, and **nothing needs two**.

## 2. The current UI vs the taxonomy

Inventory: **23 surfaces** (19 leaf pages + 3 group overviews + setup wizard),
**~250 visible controls**, **3 save models**, **5 vocabulary families**.

### 2.1 Domain C (data jobs) is shattered — the worst mismatch

One abstract domain, four screens, three vocabularies, three interaction models:

| Surface | Vocabulary | Trigger model | Pause | Cancel | Preview | Progress | History lives |
|---------|-----------|---------------|-------|--------|---------|----------|---------------|
| Replication panel (20+ controls) | "rules" | continuous + run-now | ✅ | ❌ | ❌ | per-rule | tabs inside the config panel |
| Lifecycle panel (22+ controls) | "rules" | scheduled + run-now | ❌ | ❌ | ✅ | per-rule | tabs inside the config panel |
| Re-encrypt (Buckets chips + Backends modal) | "jobs" | one-off | ❌ | ✅ | ❌ | per-bucket chip | jobs list API (no UI history) |
| Migrate bucket (modal on Buckets) | (none) | one-off **synchronous HTTP call** | ❌ | ❌ | ❌ | **none** | **none** |

An operator must learn four mental models for one concept: *"background work on
objects, with status, progress, history, and failures."* The capability matrix is
arbitrary — lifecycle can preview but not pause; replication can pause but not
cancel; migrate can do nothing and blocks an HTTP request for its whole duration.

**This mirrors the architecture exactly** (§4): three near-identical state stores
were built one feature at a time, so three UIs were built one feature at a time.
The screen fragmentation is the schema fragmentation, rendered.

### 2.2 Domain A is four screens + one orphan group

Credentials & mode / Users / Groups / External auth are correctly grouped, but
**Admission** — *pre-auth* access control, unambiguously domain A — sits as its
own top-level group containing exactly one screen.

### 2.3 Three save models, with no signal which one you're in

| Model | Screens | The user's mental contract |
|-------|---------|---------------------------|
| Section Apply (dirty → diff → Apply) | Admission, Credentials, Buckets, Replication, Lifecycle, 4× Advanced, Webhook | "nothing is live until I review" |
| Immediate mutation | Users, Groups, Backends, Recovery, providers half of Ext-auth | "every click is live" |
| Mixed **within one screen** | External auth (providers immediate, mapping rules batched) | 🤯 |

Nothing on screen tells you which contract holds. Editing a user commits
instantly; editing a bucket doesn't. The amber dirty-dot only exists for half the
screens, so its absence is meaningless.

### 2.4 Platform (E) is a grab-bag with three one-field screens

`advanced/logging` (1 input), `advanced/sync` (1 input), `advanced/limits`
(3 read-only env values) are **full navigation destinations**. Meanwhile webhook
delivery (25+ controls, clearly domain D) hides under Advanced, and
Backup/Recovery floats alone.

### 2.5 Duplicated information (the read-side tax)

Bucket facts render on 5 screens (Buckets, Backends counts, Replication +
Lifecycle selectors, Delta-efficiency dropdown); backend facts on 3; compression
on 3. Each duplicate is a place to be stale or inconsistent.

### 2.6 Vocabulary

"blocks" (admission), "rules" (replication, lifecycle, mapping), "policies"
(buckets — now invisible after the storage redesign), "jobs" (maintenance),
"providers". Five families for what the taxonomy says are three things:
*admission rules*, *job definitions*, *records*.

## 3. Proposed minimal control system

### 3.1 Target IA — six sections, 1:1 with the taxonomy (~11 surfaces, from 23)

```
Overview            health + key metrics + active-jobs strip (landing)
Access              tabs: Users & Groups · Sign-in (mode, creds, IdPs, mapping) · Admission rules
Storage             Backends · Buckets        (as shipped in the cognitive-load redesign)
Jobs                ONE screen — every background operation (see 3.2)
Integrations        Event delivery (webhook/Slack) · Event outbox (its observability)
System              one page, cards: Listener & TLS · Caches & Limits · Logging · HA sync · Backup
Diagnostics         Audit · Trace · Delta efficiency   (read-only tools)
```

Cuts: the three one-field Advanced screens fold into **System** cards; the three
group-overview pages die (the section IS the overview); Admission stops being a
top-level group; Event outbox moves next to the thing it observes.

### 3.2 The Jobs screen — the centerpiece

> Superseded (2026-10-16) by `jobs-runs-and-rules.md`: rules moved to Storage → Replication and Storage → Lifecycle; Jobs lists runs.

One table, one vocabulary, every row a **job**:

| Name | Kind | Scope | Trigger | Status | Progress | Last run | Actions |
|------|------|-------|---------|--------|----------|----------|---------|
| `mirror-to-hetzner` | Replication | beshu → hz-beshu | continuous | ● active | 412 copied | 2m ago | Pause · Run now |
| `expire-old-builds` | Lifecycle | debug/builds/ | every 1h | ‖ paused | — | 1d ago | Resume · Preview |
| `re-encrypt #3` | Re-encrypt | reenc-demo | one-off | ▶ 73% | 1973/2700 | running | Cancel |
| `migrate didi` | Migrate | didi → LOCAL1BE | one-off | ✓ done | 1.2 GB | 3d ago | — |

Row click → drawer: definition (editable, section-Apply), run history, failures.
**Uniform capability set**: every kind gets pause/resume (definitions), cancel
(running one-offs), run-now, preview/dry-run, history, failures — the capability
matrix stops being arbitrary because the machinery is shared (§4).

The Buckets busy-chips and the post-encryption proposal modal stay exactly where
they are — *launch points* belong in context; *monitoring* belongs in one place.

### 3.3 Two save models, explicitly signaled

- **Records** (users, groups, providers): saved per record, master-detail. Header
  badge: *"changes save immediately"*.
- **Configuration** (everything else): dirty → diff → Apply. Header badge shows
  the Apply contract. The dirty-dot becomes meaningful because it exists
  everywhere it can.

External auth's mapping rules move onto the Configuration contract (they are
config, not records), killing the only mixed-model screen.

### 3.4 Copy diet

Rename to the user's language: "Admission blocks" → *Admission rules*;
"Bucket policies" → done (invisible since the storage redesign); replication /
lifecycle / maintenance / migrate → *Jobs* of different kinds. Three vocabulary
families total: **rules** (admission), **jobs**, **records**.

## 4. Architectural overlaps (the cause, and the enabler)

### 4.1 The proven duplication — three copies of one machine

The inventory found `replication_*`, `lifecycle_*`, `maintenance_*` are the same
machine three times (the third copy added knowingly by the re-encrypt feature,
following house precedent):

| Machinery | Identical across the three? |
|-----------|------------------------------|
| State row + leader lease (`leader_instance_id`/`leader_expires_at`) | byte-identical SQL pattern |
| `*_try_acquire_lease` / `*_heartbeat` | identical triple |
| `*_record_failure` ring (INSERT + DELETE … NOT IN … LIMIT) | identical triple (~80 LOC) |
| Boot reconcile (zombie `running` rows) | same shape; only the policy differs (replication/lifecycle → `failed`; maintenance → re-`queued` with cursor) |
| Run/progress counters | same fields, different names (`copied` vs `affected` vs `done`) |
| Worker loop (paginate → decide-per-object → `copy_object_with_retries` → persist per page) | four divergent orchestrations of one primitive (`transfer.rs`) |

### 4.2 The odd ones out

- **Migrate bucket** (`api/admin/backends.rs`) is a *synchronous HTTP handler*
  doing exactly what a maintenance job does (bucket-level long copy) with **no**
  durability, progress, resume, or write gate — a lost client write is possible
  during migrate today for the same race the re-encrypt gate closes. It is the
  strongest candidate to become `kind: migrate` on the maintenance machinery.
- **Event delivery** is a queue-consumer (per-event rows), legitimately a
  different shape — leave it, but surface it under Integrations.
- **Lifecycle has no continuation token** (whole rule re-runs after a crash);
  it would inherit resume for free from shared machinery.

### 4.3 Consolidation roadmap (each phase independently shippable)

1. **Mechanical store dedup** (low risk, no behavior change): generic
   `record_failure_ring` / `try_acquire_leader_lease` / `renew_lease` /
   `reconcile_zombies(policy)` helpers in `config_db`; the three stores become
   thin schema bindings. ~250 LOC removed.
2. **Unified jobs read-API**: `GET /api/admin/jobs` projecting all three stores
   into one row shape (`kind`, scope, trigger, status, progress, last_run) +
   `…/:id/runs|failures`. Purely additive — enables the Jobs screen without
   touching writers.
3. **Jobs screen** (frontend): absorb ReplicationPanel + LifecyclePanel runtime
   tabs + maintenance list; definitions edit in the drawer via the existing
   section editors. Deletes two of the heaviest screens (20+ and 22+ controls).
4. **Object-job orchestrator**: decider-trait around the shared
   paginate/decide/copy/checkpoint loop; replication, lifecycle, maintenance
   workers become deciders. Lifecycle gains resume; capability matrix unifies.
5. **Migrate-as-job**: re-implement migrate on the maintenance machinery
   (durable, resumable, write-gated, progress in the Jobs screen); the admin
   endpoint becomes a job-creating POST.
6. **IA reshuffle + save-model signaling** (§3.1, §3.3) — last, because the Jobs
   screen is its prerequisite.

## 5. Scorecard

| Metric | Today | Proposed |
|--------|-------|----------|
| Navigation surfaces | 23 | ~11 |
| Mental models for background work | 4 | 1 (job) |
| Save models | 3 (one mixed in-screen) | 2, explicitly badged |
| Vocabulary families | 5 | 3 |
| Identical backend machinery copies | 3 (+1 unsafe sync outlier) | 1 |
| Screens to check "is something running?" | 4 | 1 |
