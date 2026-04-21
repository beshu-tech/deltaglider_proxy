# Admin UI Revamp — Design Plan

**Status:** Waves 1-7 shipped in v0.8.0 and its immediate follow-ons. Waves 8-10 (first-run wizard, Diagnostics dashboard + Trace UI, polish pass) still pending.

**Stakeholders:** admin UI users (three personas — solo operator,
GitOps team, GUI-first enterprise), the Rust backend team (this
plan proposes new admin API endpoints), the YAML config authors.

**Pre-reads:**
- [progressive-config-refactor.md](progressive-config-refactor.md) — the config refactor this UI revamp builds on
- [HOWTO_MIGRATE_TO_YAML.md](../HOWTO_MIGRATE_TO_YAML.md) — operator-facing migration story
- Commit range `aeb128f^..HEAD` — all Phase 3 code that ships the YAML substrate

## Progress snapshot (as of v0.8.0 + follow-ons)

| Wave | Status | Notes |
|------|--------|-------|
| 1 — Backend prep | ✅ Shipped | `GET/PUT/POST` on `/config/section/:name`, `?section=` on `/export` and `/defaults`, `GET /config/trace` query-param variant, `DGP_BOOTSTRAP_PASSWORD_HASH` env-vs-`--config` fix, RFC 7396 merge-patch on section PUT (fixed post-tag during live-browser verification). |
| 2 — Foundation components | ✅ Shipped | `FormField`, `ApplyDialog`, `MonacoYamlEditor` (lazy-loaded), `useDirtySection` hook, react-hook-form + Zod scaffolding. |
| 3 — Sidebar restructure | ✅ Shipped | 4-group IA (Diagnostics + Configuration); hierarchical URLs; legacy URL canonicalisation on mount; `RightRailActions` (Apply / Discard / Copy YAML / Export all / Import all — see scope revision below). |
| 4 — Admission section | ✅ Shipped | `AdmissionPanel` with drag-reorder, per-block Form ⇄ YAML toggle, inline validation matching server rules, synthesized `public-prefix:*` blocks surfaced read-only. |
| 5 — Access section | ✅ Shipped | Credentials & mode sub-panel; `IamSourceBanner` on every Access sub-page explaining iam_mode authority. Users / Groups / External auth kept their existing layouts, now decorated with the banner. |
| 6 — Storage section | ✅ Shipped | `BucketsPanel` with tri-state Anonymous read (None / Specific prefixes / Entire bucket → `public: true` shorthand). Test-connection button on backends. AntD 6 radio/checkbox shrink-on-click defeated in theme.css. |
| 7 — Advanced section | ✅ Shipped | Five dedicated sub-panels (Listener & TLS, Caches, Limits, Logging, Config DB sync). `🔁` restart-required badging, env-var chips on env-owned fields, fabricated TOML hints removed (pre-Phase-3 artefact). |
| 8 — First-run wizard | ⏳ Pending | — |
| 9 — Trace + Dashboard | ⏳ Pending | `POST/GET /config/trace` endpoints exist (Phase 2 + Wave 1); a Trace page and Diagnostics dashboard rendering against them are the wave's UI work. |
| 10 — Polish | ⏳ Pending | A11y, keyboard shortcuts, i18n wrappers, mobile responsive pass. |

### Scope revisions made during delivery

- **Right-rail simplification.** The plan proposed Copy YAML / Paste YAML (section-scoped) in the rail. In practice the rail ships **copy-only**; section-scoped paste lives in the `YamlImportExportModal` reached from the page header. Reason: two paste surfaces + per-section Monaco editors on-demand made "paste" functionally redundant and visually noisy.
- **IAM Backup vs. YAML Import/Export.** The sidebar entry for the encrypted-DB backup was renamed from "Backup" to **IAM Backup** so operators don't confuse it with the YAML Import/Export modal. They are structurally different: IAM Backup ships the full encrypted SQLCipher DB (users, groups, OAuth, mappings); YAML Import/Export ships the operator config document. Both live in the GUI; the sidebar naming now makes that explicit.
- **Section overview pages.** Added during live verification: every `/_/admin/configuration/<section>/` root URL now renders a lightweight overview with sub-entries + the IamSourceBanner where relevant (instead of hard-redirecting to the first sub-page), so deep-linking to a section root is a useful landing page.
- **Fabricated TOML hints removed.** The pre-Phase-3 Limits / Security / Advanced-compression forms showed copyable `TOML:` and `ENV:` example strings below every field. Many of those TOML spellings were wrong for the sectioned YAML layout; all were removed. Env-only fields now show the monospace `from DGP_X_Y` chip described in Principle 2.2 instead.

---

## 1. Executive summary

The admin UI was built before the YAML sectioned config existed. Its
9 flat sidebar tabs (Users, Groups, Authentication, Metrics, Storage,
Connection, Limits, Security, Logging) are a history of individual
features bolted on, not a structural reflection of the thing being
configured. The Phase 3 YAML refactor (`admission / access / storage
/ advanced`) has made the mismatch measurable: operators now carry
two mental models — one for YAML, one for the GUI.

This plan replaces the current IA with one that **mirrors the YAML
document 1:1**, adds **Monaco-based YAML editing** next to every
form, adopts a **plan–diff–apply** workflow for risky changes, and
introduces a **first-run wizard** so zero-to-working is four screens.

No technology is sacred. We **keep Ant Design** (shipping cost of a
full rewrite is too high to justify the incremental benefit) but
**replace** the existing hand-rolled form plumbing with
react-hook-form + Zod; we **add** Monaco + `monaco-yaml` for the
new code-view surfaces; we **add** a JSON Schema export endpoint so
both the form validators and the YAML linter drive off one source
of truth.

---

## 2. Principles

### 2.1 The UI is a window onto the YAML

Every left-nav entry corresponds to a YAML top-level key or to a
runtime diagnostic concern. Every form field has a one-line
breadcrumb showing its YAML path (e.g.
`advanced.cache_size_mb`). Every panel has a "Copy as YAML"
button that emits exactly that panel's scope.

**Mental model axiom:** an operator who has read the YAML should
recognise the GUI immediately, and vice versa.

### 2.2 Honest about authority

Following the Phase 3c `iam_mode` pattern, every screen is honest
about *who owns this state*:

- **YAML-owned** fields show a teal "YAML-managed" badge. If
  `iam_mode: declarative` is on, Users/Groups/Ext-auth flip to
  read-only with a banner explaining the escape hatch.
- **Env-var-owned** fields (anything set via a `DGP_*` var) show
  a monospace chip `from DGP_X_Y` — operator knows editing it in
  the GUI won't stick across restart unless the env var is also
  updated.
- **DB-owned** (IAM users/groups when `iam_mode: gui`) shows no
  special badge — default ownership.
- **Read-only** surfaces (synthesized admission blocks from
  `public_prefixes`, computed metrics) are visually distinct
  (locked icon, muted background).

### 2.3 Plan → Diff → Apply for risky changes

**Stolen from Nomad.** Every configuration write on a hot-reload
path runs through a three-step flow:

1. **Plan** — client-side schema check via Zod.
2. **Diff** — server-side `/config/validate` returns what would
   change + any requires_restart flags. UI shows this as a
   reviewable patch.
3. **Apply** — explicit confirmation button. Only now does the
   runtime state swap.

Auto-save on blur (the current UX) is replaced with **explicit
Apply**. Dirty state is visible in the sidebar (amber dot) and
browser tab title (`●` prefix).

### 2.4 Form and YAML are two views of one state

**Stolen from Tailscale + Rancher.** Every configuration section
(admission, access, storage, advanced, per-bucket, per-block) has
a toggle at the section header:

- **Form view** — default, Ant Design form + react-hook-form.
- **YAML view** — Monaco + monaco-yaml, seeded with the canonical
  YAML for that scope.

Switching between the views:
- Form → YAML: serialises the current form state (including
  unsaved edits) to YAML, then renders. No data loss.
- YAML → Form: parses the YAML with a tolerant parser. On parse
  failure, form view shows the parse error rather than silently
  discarding.

When the YAML contains constructs the form can't represent
(e.g. unusual `config_flag` values, future fields the form
hasn't caught up to), a yellow "too complex for the form" banner
appears with a "keep editing as YAML" affordance. Stolen from
Pomerium, but with a kinder framing.

### 2.5 Inline docs, prefills, examples everywhere

Every non-trivial input has:
- **Label** — plain-English name, not the YAML key.
- **Help text** — one sentence, below the input.
- **Default placeholder** — the runtime default shown as greyed
  placeholder (NOT a pre-filled value — we keep the invariant
  "omitted = default" intact).
- **`?` tooltip** — pulls from the JSON Schema's `description`
  field, which in turn pulls from the Rust doc comments on the
  `Config` struct. One source of truth, propagated.
- **Example chip** — clickable example value(s) for any field
  where format matters (CIDRs, globs, URIs, EnvFilter strings).
  Click inserts into the input.

This "prefill by suggestion, not by assignment" pattern keeps
the YAML minimal while answering "what do I type here?"
immediately.

### 2.6 Defaults visible, overrides obvious

Stolen from Lens. Every numeric/string input that has a non-
trivial default shows the **default value as placeholder** and
shows the **"overriding default" indicator** (small amber bar on
the left edge of the field) when the current value differs from
default. Reverting to the default is a single click. Operators
can see at-a-glance what they've customised.

---

## 3. Information architecture

### 3.1 Left nav

```
DIAGNOSTICS
├── Dashboard               /_/admin/diagnostics/dashboard
│     metrics + cache stats + admission chain preview
└── Trace                   /_/admin/diagnostics/trace
      admission debugger (POST /config/trace)

CONFIGURATION
├── Admission               /_/admin/configuration/admission
│     operator-authored block editor
├── Access                  /_/admin/configuration/access/...
│   ├── Credentials & mode      access.{authentication, iam_mode, *_key}
│   ├── Users                   DB-backed IAM users
│   ├── Groups                  DB-backed IAM groups
│   └── External authentication DB-backed OAuth/OIDC providers + mapping
├── Storage                 /_/admin/configuration/storage/...
│   ├── Backends                storage.{backend, backends, default_backend}
│   └── Buckets                 storage.buckets
└── Advanced                /_/admin/configuration/advanced/...
    ├── Listener & TLS          advanced.{listen_addr, tls}
    ├── Caches                  advanced.{cache_size_mb, metadata_cache_mb, codec_*}
    ├── Limits                  timeouts + request caps (mostly env-var)
    ├── Logging                 advanced.log_level
    └── Config DB sync          advanced.config_sync_bucket
```

**Axiom:** one top-level nav entry per YAML section + one
diagnostics group. Sub-sections are for DB-backed collections
(Users/Groups) and for grouping advanced tunables that don't
individually deserve top-level real estate.

### 3.2 URL routing

Deep-linkable, hierarchical:
- `/_/admin/configuration/admission`
- `/_/admin/configuration/access/users`
- `/_/admin/configuration/access/users/123`
- `/_/admin/configuration/storage/buckets/releases`

The URL reveals structure — operators can paste them in Slack/
issue trackers and recipients land exactly where expected.

### 3.3 Persistent right-rail actions

Visible on every configuration page:

```
┌─────────────┐
│ Apply       │   (only enabled when section dirty)
│ Discard     │   (revert to server state)
├─────────────┤
│ Copy YAML   │   (this section → clipboard)
│ Paste YAML  │   (dialog → this section editor)
├─────────────┤
│ Export all  │   (full document, opens existing modal)
│ Import all  │   (paste full YAML, opens existing modal)
└─────────────┘
```

The top pair is the dirty-state affordance; the middle pair is
the section-scoped YAML round-trip; the bottom pair is the
full-document workflow (already shipped in commit `063a649`).

---

## 4. Technology choices

### 4.1 Keep: Ant Design 6

**Rationale:** cost of rewriting every existing component is
3–4 weeks of pure churn with no user-visible improvement. The
friction points (popover z-index, tooltip disablement) are
stable workarounds. Cloudscape is technically a better fit for
operator consoles but adopting it from scratch means rebuilding
every table, form, and modal — not worth it for the incremental
gain over a well-used Ant Design installation.

**Mitigations for AntD friction:**
- Keep using `SimpleSelect` / `SimpleAutoComplete` where needed
  (documented workaround).
- Keep native `title` attributes instead of Ant tooltips on
  dense surfaces.
- Wrap Ant `Form.Item` in a thin `FormField` component that
  standardises labels, help text, placeholder behavior, and
  override-indicator rendering.

### 4.2 Adopt: react-hook-form + Zod

**Rationale:** the current UI hand-rolls validation per-field,
which means inconsistent error UX and lots of duplicated code.
react-hook-form is the dominant 2025 choice; Zod gives us a
single TypeScript-first schema definition that drives both
validation and the derived form inference.

**Migration strategy:** new forms built react-hook-form first;
existing forms migrated only when touched for other reasons.
Hybrid codebases are fine.

**Schema source:** client-side Zod schemas are the UI's source
of truth for validation messages. Server-side `schemars`-
generated JSON Schema remains the authoritative config shape
and drives the YAML view validator (see 4.3). These can diverge
on ergonomics (client-side adds "this IP is reserved for
Tailscale" kind of warnings that the server doesn't know) but
must never diverge on acceptance criteria — the server is the
last gate.

### 4.3 Adopt: Monaco + monaco-yaml

**Rationale:** every operator-console UI with YAML in it (Lens,
Rancher, GitLab, Vault) uses Monaco with `monaco-yaml` because
the "JSON Schema → live lint + hover docs + gutter squiggles"
pipeline is that good. The bundle cost (~2MB) is paid once on
the admin UI which is not a perf-sensitive entry point.

**Wiring:**
- Add `/api/admin/config/defaults` extension to scope-filter by
  section (already serves the full JSON Schema today).
- Feed `monaco-yaml` the scoped schema; each section's YAML
  editor gets **its own** schema, not the global one.
- Hover-docs pull from the Rust doc comments via `schemars`'
  `description` field.

**Not adopted:** CodeMirror 6. It's lighter but the schema
integration story is weaker; the bundle savings don't compensate.

### 4.4 Adopt: `@dnd-kit` for reorder lists

Admission blocks and IP lists need drag-reorder. `@dnd-kit` is
the modern React pick (replaced `react-dnd` and
`react-beautiful-dnd` in most 2024+ codebases).

### 4.5 Considered but not adopted

| Library/system | Why not |
|----------------|---------|
| Cloudscape | Great fit but too-high switching cost from Ant Design |
| Mantine | No compelling advantage over Ant Design for this app |
| shadcn/ui | Max flexibility but too much assembly for operator chrome |
| rjsf | Pure-schema-driven forms; loses design control; maintenance slowing |
| Formik | Being displaced by react-hook-form industry-wide |
| TanStack Form | Too young; small community |
| CodeMirror 6 | Lighter but weaker JSON Schema integration than Monaco |

---

## 5. Save/apply model

### 5.1 Three tiers

**Tier 1 — form edits (default).** Operator edits a form field.
UI marks the field + section as dirty. Apply button in the
right-rail activates. Clicking Apply:

1. Client-side Zod check.
2. PATCH to the section-level API (§ 6.2).
3. Server returns `{warnings[], requires_restart, diff}`.
4. Apply dialog shows the diff (§ 5.3) + warnings; operator
   confirms or cancels.
5. On confirm, runtime swaps. Section re-fetches.

**Tier 2 — YAML-view edits.** Operator clicks "YAML" toggle on
any section header. Monaco loads the current section's YAML.
Edits mark section dirty. Apply sends the full section YAML via
`PUT /api/admin/config/section/:name`. Same dialog, same
confirmation.

**Tier 3 — full-document apply.** Existing `Import YAML` modal.
For cross-section changes or GitOps pipeline output.

### 5.2 Dirty state UX

- **Sidebar**: amber dot next to any section with pending edits.
- **Tab title**: `●` prefix when any section is dirty.
- **beforeunload**: browser warns on unsaved changes.
- **Discard** button: reverts the current section to server state.
- **Cross-section dirty**: if two sections both have edits and
  operator Apply's one, the other stays dirty (no cross-talk).

### 5.3 The Apply dialog — the diff step

Before every Apply, the UI shows:

```
┌──────────────────────────────────────────────────────────┐
│  Review changes before applying              [Cancel]   │
├──────────────────────────────────────────────────────────┤
│  Section: advanced                                       │
│                                                          │
│  cache_size_mb                                           │
│    - 256                                                 │
│    + 2048                                                │
│                                                          │
│  log_level                                               │
│    - "deltaglider_proxy=info"                            │
│    + "deltaglider_proxy=debug,tower_http=debug"          │
│                                                          │
│  ⚠ Warnings (1)                                          │
│    • cache_size_mb is <1024 — recommend ≥1024 for prod   │
│                                                          │
│  🔁 No restart required for these changes.               │
│                                                          │
│                            [Cancel]  [Apply and Persist] │
└──────────────────────────────────────────────────────────┘
```

Stolen wholesale from Nomad. The diff is computed server-side
(`/config/validate` returns the planned diff) so the client has
no ambiguity about what will actually happen. Stolen from Kiali:
warnings are inline, not a toast.

---

## 6. REST API restructure

### 6.1 Status quo

Two admin config paths today:
- **Field-level**: `GET /api/admin/config`, `PUT /api/admin/config`
  with partial JSON deltas.
- **Document-level**: `GET /config/export`, `POST /config/validate`,
  `POST /config/apply` with full YAML docs.

### 6.2 Proposed: section-level endpoints

Add a third axis, mirroring the UI's section grouping:

- `GET /api/admin/config/section/:name` — returns the section's
  JSON (and YAML when `?format=yaml`). `:name` is one of
  `admission`, `access`, `storage`, `advanced`.
- `PUT /api/admin/config/section/:name` — partial update of that
  section. Body is JSON matching that section's shape. Runs
  through the same `apply_config_transition` helper as the
  field-level PATCH + document-level apply so all three produce
  identical hot-reload semantics.
- `POST /api/admin/config/section/:name/validate` — dry-run
  validate. Returns `{ok, warnings[], diff}` where `diff` is a
  computed JSON-patch-style list of changes vs. current.

### 6.3 Diagnostic + schema endpoints

- `GET /api/admin/config/defaults?section=:name` — scope the JSON
  Schema to a single section. Feeds Monaco's `monaco-yaml` per
  scope.
- `GET /api/admin/config/trace` (moved from POST-only) — accept
  `?method=&path=&source_ip=&authenticated=` query params for
  deep-linkable trace URLs.

### 6.4 Migration + compat

- Field-level PATCH stays for one release as a compat layer.
  UI migrates panel-by-panel to the section endpoints.
- Document-level export/validate/apply is unchanged (the GUI
  modal in `063a649` works as-is).

---

## 7. Per-section editor designs

See also the catalog at the end of this document.

### 7.1 Admission

**Layout:**
```
Admission                                   [Apply] [Discard]
Pre-auth request gating. Blocks evaluated top to bottom;
first match wins.                             Learn more →

━━━ Operator-authored blocks ━━━
[+ Add block]                       [Form view] [YAML view]

⋮⋮  deny-known-bad-ips            deny        [Edit] [×]
    source_ip_list: 2 entries

⋮⋮  maintenance-mode              reject 503  [Edit] [×]
    config_flag: maintenance_mode
    ⚠ config_flag registry not live — always false today

⋮⋮  allow-anonymous-zip-downloads allow       [Edit] [×]
    bucket: releases · methods: GET,HEAD
    path_glob: *.zip

━━━ Synthesized blocks (read-only) ━━━
Derived from Storage → Buckets' public_prefixes. Edit there.

🔒 public-prefix:docs-site        allow-anonymous
🔒 public-prefix:releases         allow-anonymous
   prefixes: builds/, stable/
```

**Block edit dialog (abridged):** see draft 2 for full detail.
Key points:
- **Name** with inline-validation matching server rules
  (reserved `public-prefix:*` prefix blocked with a link to
  "edit this bucket instead" on the Storage tab).
- **Match predicates** grouped into "Request", "Source IP",
  "Path & Bucket", "Auth state" cards — clearer than a flat
  list of 6 fields.
- **Action** radio group with conditional sub-fields for
  Reject. Destructive (deny/reject) actions get a one-sentence
  reminder "This will 403/5xx all matching requests."
- **Drag handle** (`⋮⋮`) on the list rows; `@dnd-kit` for the
  reorder.
- **Ordering hint** on top of the list: "Operator blocks fire
  before synthesized public-prefix blocks. First match wins."

### 7.2 Access → Credentials & mode

The FIRST thing on the Access section. Sets context for
Users/Groups/Ext-auth.

```
Access                                       [Apply] [Discard]

Sub-nav: [Credentials & mode*] [Users] [Groups] [Ext auth]

━━━ Credentials & mode ━━━

 IAM mode
 (•) GUI-managed
     The admin GUI and admin API write directly to the
     encrypted IAM DB. The S3 sync mechanism (advanced →
     Config DB sync) shares state across instances.
     Recommended for solo and GUI-first setups.
 ( ) Declarative
     Your YAML document is authoritative. Admin API IAM
     mutation routes return 403; YAML `access.users[]` and
     `access.groups[]` seed the DB.
     Recommended for GitOps-first teams.
 ℹ Declarative mode's reconciler (sync-diff YAML ↔ DB) lands
    in Phase 3c.3 — today, declarative mode is a lockout
    without automatic seeding. Start in GUI mode, seed your
    users, then flip to declarative.

 Authentication mode
 [Auto-detect from credentials ▼]
 Options: Auto-detect, Open access (no SigV4).

 Bootstrap SigV4 credentials
 Used before any IAM users exist and by legacy scripted
 clients. Not the same as the admin GUI password.
 Access key ID      [SURVEYKEY                         ]
 Secret key         [●●●●●●●●●●●●●●●●  ] [Reveal] [Rotate]

 Admin password
 Unlocks this GUI + encrypts the IAM DB.
 [Change password →]
```

### 7.3 Access → Users/Groups/Ext auth

Behavior depends on `iam_mode`:

- `gui`: current UsersPanel / GroupsPanel / AuthenticationPanel
  behaviour unchanged, modulo:
  - Users list gets inline "effective permissions" preview with
    a "view policy as JSON" link (semantic lint stolen from
    Kiali — flag permissions referring to nonexistent buckets).
- `declarative`: a persistent yellow banner at the top of each
  panel:
  > IAM is YAML-managed. Edit your `access.users` / `access.groups`
  > / `access.providers` and Apply. Changes here are disabled
  > until the reconciler ships in Phase 3c.3.
  
  Panels are read-only. Create/Edit/Delete buttons grayed out
  with the banner as their tooltip.

### 7.4 Storage → Backends

```
━━━ Default backend ━━━

 ( ) Filesystem
     Path [/var/lib/deltaglider                         ]
     🔁 Changing path requires restart.
 
 (•) S3-compatible
     Endpoint          [https://s3.amazonaws.com      ]
     Region            [us-east-1                     ]
     [✓] Force path style (MinIO / LocalStack)
     
     Credentials:
     ( ) None (use env vars / IAM instance profile)
     (•) Inline (stored in config; dev only)
         Access key ID [AKIA...                       ]
         Secret        [●●●●●●●●●●●●●●●●             ]
     
     [Test connection]  ← GRAFANA-INSPIRED: actually call the
                          backend, show bucket count + latency.

━━━ Named backends for multi-backend routing ━━━

 When non-empty, the default backend above is ignored.
 Pick one to be the active default.
 [+ Add named backend]
 
 ☆  europe        S3  hel1.your-objectstorage.com
 ★  primary       S3  s3.us-east-1.amazonaws.com        [Edit] [×]
 
 ★ = currently default; ☆ = available.
```

### 7.5 Storage → Buckets

Bucket list with edit dialog. The edit dialog's **Anonymous read
access** section is the key UX improvement:

```
 Anonymous read access
 ( ) None (default)
 ( ) Entire bucket
     → YAML will use the compact `public: true` shorthand.
 (•) Specific prefixes
     [builds/                              ] [×]
     [stable/                              ] [×]
     [+ Add prefix]
     Each prefix grants anonymous GET/HEAD/LIST access.
     Use trailing `/` for directory-aligned matching.
```

When operator switches between "Entire bucket" and "Specific
prefixes", the UI explains what changes in YAML. Both paths
work; operator picks by intent, not syntax.

### 7.6 Advanced (5 sub-sections)

Split into Listener/TLS, Caches, Limits, Logging, Config DB
sync (as listed in § 3.1). Each is a flat form with fields
grouped by concern. The `🔁` icon marks restart-required fields;
click → tooltip explaining the specific restart reason (e.g.
"Changing listen_addr requires the HTTP socket to re-bind").

---

## 8. First-run wizard

When the server detects:
- Empty config DB AND
- No `--config` file AND
- No file found on the default search path,

the login page shows a banner: **"First time? Start with the
setup wizard →"** instead of (or in addition to) the password
login.

**Five screens**, one question each, all with sensible defaults
prefilled:

1. **Pick a storage backend.** Filesystem (default — "Good for
   homelab and local development") or S3-compatible ("Recommended
   for production"). Previews the resulting YAML as the operator
   picks.
2. **Configure the backend.** Filesystem → path picker; S3 → URL
   + region + credentials. Includes a "Test connection" button
   that runs live. Won't let operator past until connection
   verified.
3. **Create admin credentials.** Password twice + bcrypt-hash
   preview. Explains it's both the GUI password and the DB
   encryption key.
4. **Optional: public bucket.** "Do you want to let anyone read
   one of your buckets anonymously? (e.g. docs site, public
   release feed)" — text field + `public: true` toggle. Skippable.
5. **Review.** Show the generated YAML in a Monaco read-only
   pane with "Copy" button. "Save and start" → POSTs
   `/config/apply`, redirects to Dashboard.

**Hard rules:** no step has more than 3 inputs. Every input
shows its default. Back/Next buttons always visible. Skip button
where legitimate. Total time to complete: under 3 minutes.

---

## 9. Diagnostics

### 9.1 Dashboard

Lands here post-login. Composition:

- **Top row — health cards**: Backend status (green/yellow/red),
  cache hit rate, request rate, compression savings. Each
  clickable → deep-link into the relevant config section.
- **Admission chain preview**: the full chain in order
  (operator-authored + synthesized), each block collapsible.
  Click → Admission tab.
- **Tainted fields banner**: if any config field diverges from
  the on-disk YAML, surface it here. Clickable — jumps to the
  specific field.
- **Recent audit**: last 50 entries from the audit log.
- **Version + uptime footer**.

### 9.2 Trace

Form + result stacked:

```
━━━ Trace admission decision ━━━

 Method           [GET ▼]
 Path             [/releases/builds/v1.zip             ]
                  Example: `/mybucket/some/key.zip`

 Query string     [prefix=builds/                      ]
                  Optional — LIST `prefix=` parameter.

 Source IP        [203.0.113.5                         ]
                  Leave empty to test with no IP.

 [✓] Authenticated (request carries SigV4)

                                          [Run trace]

━━━ Result ━━━

 Decision: deny
 Matched:  deny-known-bad-ips
 
 Reason path (request → block → action):
 GET /releases/builds/v1.zip from 203.0.113.5
   → deny-known-bad-ips (source_ip_list matches 203.0.113.0/24)
   → action: deny
   → response: 403 Forbidden (S3-style XML)
 
 Chain at time of trace:
 (full chain rendered with matched block highlighted)
```

The trace page is a one-click debug tool that every support
ticket can start with. The "reason path" is the key UX — stolen
from Istio's Kiali "validation trace" view.

---

## 10. Polish

### 10.1 Accessibility

- All interactive elements keyboard-reachable with visible focus
  rings.
- Every icon has an `aria-label`.
- Monaco has built-in screen-reader support; we verify it's
  enabled.
- Form errors announced via `aria-live="polite"`.
- Colour contrast meets WCAG 2.1 AA in both light and dark
  themes.

### 10.2 i18n readiness

- All strings go through a single `t(key)` helper (even if it
  just returns English today). Sets us up for locale addition
  without another rewrite.

### 10.3 Keyboard shortcuts

- `⌘K` / `ctrl-K`: command palette (quick nav to any section).
- `⌘S` / `ctrl-S`: trigger Apply when section is dirty.
- `⌘⇧Y`: toggle Form/YAML view on the current section.

Documented in a Shortcuts help modal (accessible via `?`).

### 10.4 Mobile

Admin UI is not mobile-optimised today and we don't promise it
will be. Target: sidebar collapses to drawer at <900px; forms
remain usable at 600px; Monaco becomes read-only on mobile
(editing YAML on a phone is a bad idea).

---

## 11. Rollout — 10 waves

| Wave | Duration | Scope | Delivers |
|------|----------|-------|----------|
| 1 — Backend prep | 1 week | Section-level `/config/section/:name` GET/PUT/validate endpoints; `?section=` filter on `/config/export` and `/config/defaults`; trace query-param variant | Foundation for 2–9 |
| 2 — Foundation components | 1 week | Monaco + monaco-yaml wired; react-hook-form + Zod scaffolding; `FormField` wrapper; `ApplyDialog` component | Reusable across every section |
| 3 — Sidebar restructure | 3 days | New 4-group IA; URL routing; right-rail actions | Visible win even before content changes |
| 4 — Admission section | 1 week | Block editor + drag reorder + per-block form & YAML views | Highest-value new UI |
| 5 — Access section | 1 week | Credentials/mode sub-section; declarative banner; Users/Groups/Ext-auth remain but gain iam_mode awareness | Phase 3c story complete |
| 6 — Storage section | 3 days | Backend + Buckets sub-sections; `public: true` toggle; Test connection button | Fixes the current "Storage + Connection" tab split |
| 7 — Advanced section | 2 days | 5 sub-sections; restart-required badging; env-var chips | Surface the whole knob catalog |
| 8 — First-run wizard | 3 days | 5-screen flow; YAML preview on last screen | Zero-to-working in 3 min |
| 9 — Trace + Dashboard | 3 days | Diagnostics group; reason-path UX | Debug-first operator experience |
| 10 — Polish | 1 week | A11y pass, keyboard shortcuts, i18n wrappers, mobile responsive pass | Production-ready |

**Total: ~6 weeks of focused work.** Each wave is an
independently-shippable PR; incremental delivery means the UI
keeps working throughout.

---

## 12. Risks + mitigations

| Risk | Mitigation |
|------|------------|
| Monaco bundle size (~2MB) slows admin page | Lazy-load Monaco only when YAML view opens; code-split |
| Form ↔ YAML sync bugs drop data | Property-based test: fuzz arbitrary `Config` → form → YAML → form → YAML, assert stable after first round-trip |
| Ant Design popup bugs resurface on new panels | Keep `SimpleSelect` / `SimpleAutoComplete` discipline; PR template has a "tested in modal overlay?" checkbox |
| Section-level PATCH drifts from document-level apply | All three paths route through `apply_config_transition`; golden test asserts identical behavior for representative diffs |
| Wizard inputs don't cover edge cases (e.g. S3 with custom CA) | Wizard has a "skip and import YAML instead" exit door on every screen |
| Declarative-mode banner confuses operators | A/B test banner copy with two operators before rollout |
| ConfigDB bootstrap-password env-var bug (surfaced during survey) | Fix in Wave 1 alongside the backend prep |

---

## 13. Success metrics

How we'll know this worked:

1. **Time-to-first-save**: measured from "operator lands on the
   login page" to "operator successfully applies their first
   config change". Target: under 5 minutes on a fresh install
   (vs. current ~20 min).
2. **"Where is X in the GUI?" support questions**: tracked in
   the issue tracker as a tag. Target: 80% reduction over the
   first 3 months post-launch.
3. **YAML/GUI round-trip delta**: automated test suite that
   applies a curated set of YAML configs via the document API,
   then exports from the UI, asserts the exported YAML loads
   cleanly. Target: 100% of representative configs.
4. **Operator NPS on the admin UI**: one survey question shipped
   in the product. Target: +30 absolute improvement.

---

## 14. What this plan does NOT do

- Does not touch the `/browse` S3 object browser.
- Does not redesign the Metrics dashboard (too much analytics
  logic to justify churn — ship as-is within the new nav).
- Does not ship the Phase 3c.3 reconciler (that's a backend
  feature the UI waits for; the declarative banner explains
  the gap).
- Does not deliver i18n strings beyond the `t()` wrapper.
- Does not replace Ant Design.

---

## Appendix A — Config surface catalog

Sourced from an exhaustive code scan; reproduced here so the UI
designer knows every field that exists. See the full catalog in
the session transcript or regenerate via the Explore agent
against `src/config.rs`, `src/config_sections.rs`,
`src/admission/spec.rs`, `src/bucket_policy.rs`.

### Admission section

| Field | Type | Default | Hot-reload | Dangerous | One-liner |
|-------|------|---------|------------|-----------|-----------|
| `blocks[]` | Vec | `[]` | Y | Medium | Operator-authored admission rules |
| `.name` | String | req | Y | N | Unique block identifier, 1-128 chars `[A-Za-z0-9_:.-]` |
| `.match.method` | `Vec<String>` | any | Y | N | HTTP methods — GET/HEAD/PUT/POST/DELETE/PATCH/OPTIONS |
| `.match.source_ip` | IP | any | Y | M | Exact source IP (mutually exclusive with source_ip_list) |
| `.match.source_ip_list` | `Vec<IP\|CIDR>` | any | Y | M | Source IPs/CIDRs, max 4096 entries |
| `.match.bucket` | String | any | Y | N | Target bucket (lowercased) |
| `.match.path_glob` | Glob | any | Y | N | Object-key glob — `*.zip`, `releases/**` |
| `.match.authenticated` | `Option<bool>` | any | Y | N | Only auth / only anon / either |
| `.match.config_flag` | String | — | Y | N | Named flag (registry not yet live) |
| `.action` | enum | req | Y | M | allow-anonymous / deny / reject / continue |
| `.action.reject.status` | u16 | req | Y | H | 4xx/5xx only |
| `.action.reject.message` | String | — | Y | N | Response body for reject |

### Access section

| Field | Type | Default | Hot-reload | Dangerous | One-liner |
|-------|------|---------|------------|-----------|-----------|
| `iam_mode` | enum | gui | N | H | gui or declarative |
| `authentication` | `Option<String>` | auto | N | H | "none" / absent = auto-detect |
| `access_key_id` | `Option<String>` | — | Y | H | Bootstrap SigV4 key |
| `secret_access_key` | `Option<String>` | — | Y | H | Bootstrap SigV4 secret |

### Storage section

| Field | Type | Default | Hot-reload | Dangerous | One-liner |
|-------|------|---------|------------|-----------|-----------|
| `backend` | tagged union | Filesystem | N | H | Default single backend |
| `.type=s3` | — | — | N | H | — |
| `.endpoint` | String | — | N | H | S3 endpoint URL |
| `.region` | String | us-east-1 | N | N | AWS region |
| `.force_path_style` | bool | true | N | N | MinIO/LocalStack friendly |
| `.access_key_id` | `Option<String>` | env | N | H | Backend-scoped S3 key |
| `.secret_access_key` | `Option<String>` | env | N | H | Backend-scoped S3 secret |
| `.type=filesystem` | — | — | N | H | — |
| `.path` | PathBuf | `./data` | N | H | Data directory (`..` rejected) |
| `backends[]` | `Vec<NamedBackend>` | `[]` | N | H | Multi-backend routing |
| `default_backend` | `Option<String>` | first | N | H | Which named backend is the default |
| `buckets.{name}.compression` | `Option<bool>` | global | Y | N | Per-bucket compression override |
| `buckets.{name}.max_delta_ratio` | `Option<f32>` | global | Y | N | Per-bucket ratio override |
| `buckets.{name}.backend` | `Option<String>` | default | N | M | Route this bucket to named backend |
| `buckets.{name}.alias` | `Option<String>` | — | N | M | Virtual→real bucket name map |
| `buckets.{name}.public_prefixes[]` | `Vec<String>` | `[]` | Y | M | Anonymous-read key prefixes |
| `buckets.{name}.public` | `Option<bool>` | — | Y | M | Shorthand for `public_prefixes: [""]` |
| `buckets.{name}.quota_bytes` | `Option<u64>` | — | Y | M | Soft storage quota |

### Advanced section

| Field | Type | Default | Hot-reload | Dangerous | One-liner |
|-------|------|---------|------------|-----------|-----------|
| `listen_addr` | SocketAddr | `0.0.0.0:9000` | N | H | HTTP listen address |
| `max_delta_ratio` | f32 | 0.75 | Y | N | Global default |
| `max_object_size` | u64 | 100 MiB | Y | N | xdelta3 mem cap |
| `cache_size_mb` | usize | 100 | Y | N | Reference cache |
| `metadata_cache_mb` | usize | 50 | Y | N | Object metadata cache |
| `codec_concurrency` | `Option<usize>` | auto | N | N | xdelta3 concurrent subprocs |
| `blocking_threads` | `Option<usize>` | 512 | N | N | tokio blocking pool |
| `log_level` | String | `deltaglider_proxy=debug` | Y | N | EnvFilter string |
| `config_sync_bucket` | `Option<String>` | — | N | H | S3 bucket for IAM DB sync |
| `tls.enabled` | bool | false | N | N | Enable HTTPS |
| `tls.cert_path` | `Option<String>` | — | N | N | PEM cert path |
| `tls.key_path` | `Option<String>` | — | N | N | PEM key path |
| `bootstrap_password_hash` | `Option<String>` | auto | N | H | bcrypt hash (infra secret) |
| `encryption_key` | `Option<String>` | — | N | H | AES-256 at-rest (infra secret) |

### Runtime state (admin API, not YAML)

DB-backed IAM state + diagnostic surfaces — not appropriate for
YAML editing (mostly; `iam_mode: declarative` changes the
authority, not the shape). See the full admin-API route list in
the session transcript.

---

## Appendix B — References

- [progressive-config-refactor.md](progressive-config-refactor.md) — the config refactor plan this builds on
- Grafana, Pomerium, Cloudflare Zero Trust, Tailscale, Lens/Rancher,
  Traefik, Istio/Kiali, MinIO Console, Nomad/Consul, Vault —
  exemplar products studied for patterns
- Libraries: [react-hook-form](https://react-hook-form.com/) +
  [Zod](https://zod.dev/) +
  [Monaco](https://microsoft.github.io/monaco-editor/) +
  [monaco-yaml](https://github.com/remcohaszing/monaco-yaml) +
  [@dnd-kit](https://dndkit.com/)
