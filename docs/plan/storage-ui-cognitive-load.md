# Storage admin UI — cognitive-load study (Backends + Bucket policies)

*PM/design investigation, June 2026. Evidence: live screenshots of a representative
2-backend / 4-bucket deployment, plus an exhaustive control-and-concept inventory of
`BackendsPanel`, `BackendEncryptionEditor`, `BucketsPanel`, `BucketCard`,
`PrefixListEditor`, `CreateBucketModal`, `MigrateBucketModal`.*

## The numbers (what the operator's eyes must process)

| Measure | Backends page | Buckets page (3 policies) |
|---|---|---|
| Interactive controls visible at once | ~15 (+8–11 when the Add form opens) | **~30** (9–11 per card, all expanded, always) |
| Distinct vocabulary terms on screen | ~15 (backend, default, legacy singleton, routed, encryption mode ×4, key id, shim, path-style…) | ~12 (policy, route, compression override, ratio, alias, quota, anonymous read, prefix, SigV4, `public: true`…) |
| Concepts needed for the top-5 tasks | ~8 | — |
| Microcopy strings explaining the model | ~40 across both pages | — |

**~30 concepts exposed; ~8 needed for the common tasks. Roughly 70% of what's on
screen is tail-case knowledge billed to every visitor on every visit.** Forty strings
of explanatory microcopy is the system compensating in prose for a model that's too
visible — every one of those sentences is a thought we charge the user.

## Findings, ranked by cognitive cost

### F1 — The single worst defect: effective values are invisible (forced mental computation)
On the Buckets page, bucket `beshu` routes to the default backend — so its Route
select shows **empty placeholder "Route to…"**. The answer to *"where does beshu's
data live?"* is not on the page. The user must recall that empty = default, then
recall which backend is default (that fact lives on the *other* page). Same pattern:
`Ratio` placeholder says `global` (what is the global? 3 levels of indirection to
0.75), `Quota` says `unlimited`, compression says "Default (on)". **Rule violated:
recognition over recall.** Every inherited field makes the user *compute* the
inheritance chain in their head.

### F2 — The "policy row" is an implementation leak
The Buckets page lists *policy rows*, not buckets. A bucket with no overrides does
not exist on the page, so the operator's most basic read question — "what's the
state of bucket X?" — frequently has no answer surface. Worse: the row's *name* is
an editable autocomplete, so a policy can be silently re-pointed at a different
bucket (or a non-existent one) by editing text. The user must maintain the
distinction "policy ≠ bucket" purely to navigate a serialization detail
(`storage.buckets.<name>` map keys). The page even ends with a footer explaining
YAML round-tripping of `public: true` vs `public_prefixes` — serialization trivia
as permanent furniture.

### F3 — Flat wall of expert controls, no progressive disclosure
Every bucket card always shows: name, route, delete, compression mode, delta
ratio (0–1, step 0.05!), encryption badge, alias, quota, and a 3-option public-access
radio group with inline prefix editor. The live deployment shows **0 quotas, 0
aliases, 0 ratio overrides** — yet those controls occupy a third of every card,
at the same visual rank as "make this bucket public to the internet". Radio groups
are also being used as *status display* (to read whether a bucket is public you
scan three radios), which is form furniture doing a chip's job.

### F4 — Three save models in one storage section (unpredictability is the most expensive load)
- Global compression toggle: **applies instantly**, no confirm — despite having the
  largest blast radius on the page.
- Create/delete backend: instant, `window.confirm()` for delete.
- Encryption: its own local edit→checkbox→Apply flow.
- Everything on Buckets: staged → dirty banner → "Review apply" → diff modal.

Same config section, four commit rituals. The user cannot predict whether a click
is already live. The heavyweight diff modal guards a quota number while the
zero-friction toggle flips compression for every future bucket.

### F5 — Page ordering and duplicated surfaces
The page named **Backends** opens with… a global *compression* card (not a backend
concern, not a top task). It closes with a "Bucket policy routing" stats card
(Policies/Routed/Public/Quotas) that duplicates the other page and adds a 4-term
mini-vocabulary — a cross-reference grown to compensate for the two-page split of
one mental model ("where does data live + who can see it").

### F6 — Same fact, two vocabularies; warnings as wallpaper; unlabeled icons
- Encryption row: status says `Encryption: DISABLED`, the control beside it says
  `None (plaintext)` — one fact, two words, the user reconciles them.
- "Routing only — won't move existing objects." is permanently printed under every
  routed bucket — a warning about an action not being taken (banner blindness; the
  re-route confirm already covers the actual action).
- Test-connection and delete are unlabeled icon-only buttons; "Force path-style
  URLs" defaults ON with zero "do I care?" guidance (rule of thumb: ON for
  S3-compatibles, OFF for AWS — say that).
- Option labels carry provenance trivia: "On — explicit in YAML".

### F7 — Journeys cross pages and end blind
Add backend → create bucket → make a prefix public is the canonical onboarding
journey: it spans both pages, and creating an S3 backend performs **no connection
test** — you save blind, then discover the plug icon afterwards.

## What good looks like ("make people think less")

**Design target: a read task costs zero form controls; an edit task exposes only its
own controls; every value shown is the effective value; one commit ritual.**

### R1 — Buckets page lists ALL buckets as status rows (kill the visible "policy" concept)
Source the list from bucket origins (already cached for the sidebar/counts). One
compact row per real bucket: `name · backend chip · encryption chip · public chip ·
quota chip (when set)`. Buckets without overrides simply show all-inherited chips.
"Add bucket policy" disappears; rows aren't name-editable (the bucket *is* the row).
Policies become invisible serialization. *(This is the structural fix for F1+F2+F3.)*

### R2 — Expand-to-edit with three groups
Click a row to expand: **Access** (public tri-state + prefixes — amber, the one
dangerous group), **Placement** (backend + "Migrate data…"), **Advanced** (collapsed
disclosure: compression override, delta cutoff, alias, quota). Collapsed cost of the
page for N buckets: N status lines, 0 inputs.

### R3 — Effective values everywhere
Route select renders `Default — HetznerHelsinki1` when unset; ratio placeholder
renders the resolved number (`0.75 — global default`); quota `Unlimited`. Provenance
as a suffix, value always concrete. *(Cheap; highest value-per-line in this list.)*

### R4 — One save model for the whole Storage section
Adopt the staged/diff model uniformly: the compression toggle stages like everything
else; backend create/delete keep immediate semantics but get real modals (delete
must say "2 buckets route here — they'll fall back to default reads/writes").
One ritual to learn, applied everywhere the word "storage" appears.

### R5 — Copy diet
- "Anonymous read access" → **"Public access"**.
- Compression options → `Inherit (on)` / `Always on` / `Off`.
- "Alias" → under Advanced as "Real bucket name on backend".
- Delete the YAML round-trip footer and the permanent "Routing only" caption (the
  re-route confirm carries that load at the right moment).
- Encryption row states one vocabulary: chip `Not encrypted` + button "Enable…".

### R6 — Backends page: backends first, defaults last, stats gone
Order: backend cards → Add backend → a small "Defaults" card (compression toggle +
default backend selector together — they're the same kind of thing). Delete the
"Bucket policy routing" stats card; the per-backend "N buckets routed here — view"
line (already present) is the better, contextual version of it.

### R7 — Backend card = one status line + on-demand editors
`HetznerHelsinki1 · S3 hel1 · DEFAULT · 3 buckets · Not encrypted` with labeled
actions. The encryption editor mounts on demand instead of rendering a permanent
sub-card per backend.

### R8 — Close the journey loops
Create-backend (S3) runs the connection test as part of Create (fail = nothing
saved, error shown inline). On success, the form's success alert offers the next
step: "Create a bucket on `<name>` →" (the modal already exists and accepts
`presetBackend`).

## Priority / effort

| Move | Load removed | Effort |
|---|---|---|
| R3 effective values | F1 (the worst) | XS — copy + display logic |
| R5 copy diet | F6, part of F3 | XS |
| R6 page order + delete stats card | F5 | S |
| R8 test-on-create + chained next step | F7 | S |
| R4 one save model | F4 | M |
| R1+R2 status-row buckets page | F2, F3, F1 | M–L (the structural win) |
| R7 backend card consolidation | F6 | M |

Projected effect of R1–R3 alone: Buckets page drops from ~30 always-visible inputs
and ~12 terms to **N status rows, 0 inputs, ~5 terms**, and every read question
("where does X live, is it public, is it encrypted, any quota?") is answered by one
glance at chips showing *effective* state.
