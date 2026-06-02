# Webhook GUI — make event-delivery config GUI-editable

## Context

Event-delivery (the webhook side of the durable `event_outbox`) is the one config
surface that is still **YAML/env-only**: there is no admin GUI to edit it. Operators
can already *observe* delivery via the **Event outbox** diagnostics panel
(`diagnostics/event-outbox` → `EventOutboxPanel`, backed by
`GET /_/api/admin/event-outbox`), but to turn webhooks on, point them at an endpoint,
add an auth header, or tune retry/retention they must hand-edit YAML and reload.

This plan adds a **Configuration → Advanced → Webhook delivery** panel that edits
`EventDeliveryConfig` through the exact same section-editor pipeline every other config
panel uses. It is the natural companion to the just-shipped event-driven replication:
both are independent listeners on the same outbox.

**Scope:** edit the existing `EventDeliveryConfig` (no new backend config shape). The
backend route, validation, schema, and hot-reload already exist — this is ~95% a
frontend panel + nav wiring, plus a small "test delivery" affordance.

## What already exists (verified, do NOT rebuild)

- **Config struct** `EventDeliveryConfig` — `src/config_sections.rs:604-666`, nested at
  `AdvancedSection.event_delivery` (`:1029-1034`). 13 fields:
  `enabled: bool`, `webhook_url: Option<String>`, `webhook_urls: Vec<String>`,
  `webhook_headers: BTreeMap<String,String>`, `tick_interval`, `batch_size`,
  `request_timeout`, `max_attempts`, `retry_base`, `retry_max`, `stale_claim_after`,
  `delivered_retention`, `delivered_max_rows`, `prune_batch`. Already `#[derive(JsonSchema)]`.
- **Defaults** — `src/config_sections.rs:705-743` (enabled=false, tick=10s, batch=50,
  timeout=5s, max_attempts=8, retry_base=5s, retry_max=5m, stale_claim=60s,
  retention=24h, delivered_max_rows=10_000, prune_batch=100).
- **Validation** — `validate_event_delivery()` `src/config_sections.rs:1567-1652`
  (URL parse, header name/value, humantime durations, numeric ranges). Runs inside
  `Config::check()` on every section apply. Nothing to add.
- **Gating accessors** — `is_active()` (`enabled && !endpoints.is_empty()`),
  `webhook_endpoints()` (merges `webhook_url` + `webhook_urls`, trims, drops empties)
  — `src/config_sections.rs:668-681`.
- **Backend section route** — `PUT /api/admin/config/section/advanced` (RFC 7396
  merge-patch), `GET .../advanced`, `POST .../advanced/validate` (dry-run) —
  `src/api/admin/config/section_level.rs:132-630`. Hot-reloads: the dispatcher
  re-reads `SharedConfig` every tick (`src/event_delivery.rs:122-129`), so changes
  apply with **no restart**.
- **Outbox viewer** — `GET /_/api/admin/event-outbox` returns rows + status counts +
  `delivery_enabled` / `delivery_active`; `POST .../:id/requeue` and bulk requeue —
  `src/api/admin/event_outbox.rs:50-193`. Frontend `EventOutboxPanel.tsx` at
  `diagnostics/event-outbox`.
- **JSON Schema** — auto-generated via `schemars::schema_for!(Config)`
  (`src/cli/config.rs:92-107`); CI "Config JSON Schema" job regenerates it. No new
  fields here → schema is already correct.

## Frontend pattern to mirror (exact anchors)

- **`useSectionEditor<Wire, Local>`** — `demo/s3-browser/ui/src/useSectionEditor.ts:115-247`.
  The single home for fetch → dirty → validate → ApplyDialog → PUT → markApplied. Use
  `section: 'advanced'`, `pick: (body) => body.event_delivery`,
  `toPayload: (v) => ({ event_delivery: v })`.
- **Exemplar panel** — `ReplicationPanel.tsx:1-425` (best match: edits a nested
  advanced/storage struct with arrays, has Advanced disclosure for optional fields,
  guarded apply, `useApplyHandler` for ⌘S, ApplyDialog wiring). Copy its skeleton.
  `LoggingPanel.tsx` is the *minimal* exemplar (single nested field) if a leaner start
  is wanted.
- **Nav entry** — add to `ADMIN_IA` advanced children array, after the `sync` entry at
  `adminNavigation.tsx:266`. Shape (matches siblings exactly):
  ```tsx
  {
    path: 'configuration/advanced/event-delivery',
    label: 'Webhook delivery',
    icon: <SendOutlined />,            // import from @ant-design/icons
    section: 'advanced',
    dirtyKey: 'configuration/advanced/event-delivery',
    description:
      'Durable event webhook delivery: endpoints, auth headers, retry backoff, retention. Applies live, no restart.',
  },
  ```
- **AdminPage routing** — add a branch alongside the other advanced ones at
  `AdminPage.tsx:730` (after the `sync` block):
  ```tsx
  if (adminPath === 'configuration/advanced/event-delivery') {
    return (<>{header}<WebhookDeliveryPanel onSessionExpired={onSessionExpired} /></>);
  }
  ```
  Import `WebhookDeliveryPanel` near the other panel imports (`AdminPage.tsx:21-27`).
  `LEGACY_TO_NEW` (`AdminPage.tsx:90-92`) needs **no** entry (this is a brand-new page,
  no legacy flat URL to alias).

## Implementation

### Phase 1 — Frontend types + panel (the bulk)
1. **Wire types** in `s3client.ts` (or wherever section bodies are typed): an
   `EventDeliveryConfig` TS interface mirroring the 13 fields + a `DEFAULT_EVENT_DELIVERY`
   const matching the Rust defaults. Add `event_delivery` to the `AdvancedSectionBody`
   type used by the advanced-section editors.
2. **`WebhookDeliveryPanel.tsx`** (new), copying `ReplicationPanel`'s skeleton:
   - `useSectionEditor<AdvancedSectionBody, EventDeliveryConfig>({ section:'advanced',
     dirtyKey:'configuration/advanced/event-delivery', initial: DEFAULT_EVENT_DELIVERY,
     pick: b => b.event_delivery ?? DEFAULT_EVENT_DELIVERY,
     toPayload: v => ({ event_delivery: v }), noun:'webhook delivery', onSessionExpired })`.
   - **Primary fields**: `enabled` (Switch), `webhook_url` + `webhook_urls`
     (a single editable string-list — model as one list internally and split the first
     into `webhook_url` on `toPayload`, OR just always write `webhook_urls` and leave
     `webhook_url` for legacy; simplest is to surface ONE "Endpoints" list bound to
     `webhook_urls` and keep `webhook_url` as a read-through). **Decide: collapse both
     into a single Endpoints list editing `webhook_urls`; map a lone legacy `webhook_url`
     into the list on load and clear it on save.** Mirror `ConditionPrefixInput`-style
     add/remove-row with stable index keys (see the admin-editor bug-class memory — avoid
     comma round-trip / array-index key bugs).
   - **Headers**: key/value map editor bound to `webhook_headers` (same add/remove-row
     pattern; values may be bearer tokens → mask on display, see secret note below).
   - **Advanced disclosure** (collapsed by default, like ReplicationPanel's): the tuning
     fields — `tick_interval`, `batch_size`, `request_timeout`, `max_attempts`,
     `retry_base`, `retry_max`, `stale_claim_after`, `delivered_retention`,
     `delivered_max_rows`, `prune_batch`. Each `FormField` with YAML-path breadcrumb +
     default-placeholder (reuse the shared `FormField`).
   - **Apply flow**: guarded client-side validation (non-empty endpoint when enabled;
     durations look like humantime) → `editorRunApply()` → `ApplyDialog` → confirm.
     Register `useApplyHandler` for ⌘S.
   - **Live delivery strip** at top: fetch `GET /_/api/admin/event-outbox?limit=1` for
     `delivery_enabled` / `delivery_active` + status counts; render a compact
     "Active / Inactive · N pending · N failed" banner with a link to
     `diagnostics/event-outbox` for the full table. (Read-only; reuses existing route.)

### Phase 2 — "Send test event" affordance (small, high-value)
A button that POSTs a synthetic event to the configured endpoint(s) so an operator can
confirm wiring without mutating an object. Two options — pick per the no-test-route rule
in CLAUDE.md (endpoints ship only for a real production use case; a test-ping IS a real
operator affordance, like `config/sync-now`):
- **Preferred:** `POST /_/api/admin/event-outbox/test` — server builds a
  `deltaglider.event.v1` payload with a sentinel key (e.g. `__webhook_test__`) and calls
  the existing `HttpWebhookDeliveryClient::deliver` against the *current* config,
  returning per-endpoint status. No DB row inserted (pure delivery probe). This mirrors
  `sync-now` as a legitimate operator affordance and keeps the test path identical to
  production delivery.
- Frontend: a "Send test event" button in the panel → shows per-endpoint result chips.

### Phase 3 — Docs + verification
- Update `deltaglider_proxy.example.yaml` comment for `advanced.event_delivery` to point
  at the GUI; mention it's hot-reloaded.
- Frontend gate: `npm run lint && tsc && knip` (knip will flag the new component if not
  wired — it IS wired via AdminPage import). Add the new panel to any barrel if needed.
- `cargo build` (only if Phase 2 backend route added) + `cargo clippy -D warnings`.
- Node regression: if the endpoints/headers list editors get pure helpers (split/merge,
  dedupe), colocate a Node regression test like the `ConditionPrefixInput` reference fix
  (admin-editor bug-class memory).
- E2E smoke: extend an embedded-UI smoke if the existing one covers admin panels.

## Risks / gotchas (verified)
1. **Secret round-trip.** `GET .../advanced` redacts secrets; `webhook_headers` values
   (bearer tokens) and any credential-bearing `webhook_url` must survive GET→edit→PUT
   untouched when the operator doesn't retype them. Check how `section_level.rs:305-406`
   preserves runtime secrets and ensure header VALUES are in that preservation set (they
   may not be today — if a header value comes back redacted and is PUT verbatim, it would
   overwrite the real token with the redaction sentinel). **This is the one backend item
   to verify/extend.** If header values aren't preserved, add them to the preserve list
   (mirror `preserve_sigv4_pair`).
2. **`webhook_url` vs `webhook_urls` duplication.** Collapsing to one Endpoints list
   avoids a confusing two-field UI; make the mapping lossless (legacy single URL → list
   on load) so existing YAML configs round-trip.
3. **Array-index keys / comma round-trip** in the endpoints + headers list editors — the
   recurring admin-editor bug class. Use the `ConditionPrefixInput` pure-helper + stable-id
   pattern and a Node regression test.
4. **Don't add a test-only route.** The Phase-2 test-ping is justified ONLY as a genuine
   operator affordance (confirm webhook wiring in prod). Frame it as such or drop it.

## Effort
~1–1.5 days. Phase 1 (panel + nav + types) is the bulk and is pure frontend mirroring an
existing panel. Phase 2 is a small optional backend route + button. No new config shape,
no schema work, no migration, hot-reload already works.
