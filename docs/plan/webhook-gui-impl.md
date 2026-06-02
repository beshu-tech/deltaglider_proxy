# Webhook GUI — implementation checklist (execution)

Scope locked with user: **config panel only** (no test-ping route). Header values
treated as secrets with **preserve-on-untouched** UX. "Usability bugs ARE bugs."

## Backend (Rust)

### B1. Redact webhook header values + credential-bearing URL on GET
`src/config.rs::redact_all_secrets` (~2007). Header VALUES can carry bearer tokens
→ redact them to a sentinel so the GUI shows masked, and the operator never sees
the real token. Decision: use an explicit **string sentinel** `"__redacted__"`
(NOT `None`-omit, because the map needs to keep its KEYS so the UI can show which
headers exist while masking values).
- Add: for `export.event_delivery.webhook_headers`, replace every value with
  `REDACTED_SENTINEL`. Keep keys.
- `webhook_url` / `webhook_urls`: endpoints are not secrets per se, but may embed
  `https://user:tok@host`. Leave URLs visible (operators need to see/verify the
  endpoint); document that credentials belong in headers, not the URL. (No redaction
  on URLs — keeps the editor honest.)
- Define `pub const REDACTED_SENTINEL: &str = "__redacted__";` somewhere shared
  (config.rs) so the preserve helper + tests reference one constant.

### B2. Preserve unredacted header values on PUT
`src/api/admin/config/section_level.rs::apply_section` preserve block (~305-406),
+ a helper in `config/mod.rs` next to `preserve_sigv4_pair`.
- New `pub(super) fn preserve_event_delivery_secrets(new: &mut EventDeliveryConfig,
  old: &EventDeliveryConfig)`: for each header in `new.webhook_headers`, if value ==
  `REDACTED_SENTINEL`, replace with `old.webhook_headers.get(key)` (drop the entry if
  the old map lacks it — a redacted value for a brand-new key is nonsense, treat as
  "unset"). Headers the operator actually retyped (value != sentinel) pass through.
  Headers the operator REMOVED are simply absent from `new` (merge-patch handled it).
- Call it in the preserve block alongside the sigv4 calls.

### B3. Merge-patch foot-gun: header DELETE must produce explicit nulls
The RFC 7396 merge applies key-by-key; an absent key is PRESERVED, only an explicit
`null` deletes. The FRONTEND `toPayload` must therefore diff against the loaded
baseline and emit `null` for removed header keys. (Backend already supports it —
this is a frontend obligation, see F4.) Add a backend test proving
`{"webhook_headers":{"X":null}}` deletes X.

### B4. Tests (Rust)
- Unit/integration in `admin_section_test.rs`: (a) GET redacts header values to
  sentinel, keeps keys; (b) PUT with sentinel value preserves old token; (c) PUT
  with a real new value overwrites; (d) PUT with `null` value deletes the header;
  (e) full round-trip GET→unchanged PUT leaves tokens intact.
- Pure-fn unit test for `preserve_event_delivery_secrets` truth table (no server).

## Frontend (React/TS)

### F1. Types
`adminApi.ts` (or s3client/adminApi): `EventDeliveryConfig` interface (13 fields),
`DEFAULT_EVENT_DELIVERY` matching Rust defaults, `AdvancedSectionBody` gains
`event_delivery?: EventDeliveryConfig`. Export `WEBHOOK_REDACTED_SENTINEL = "__redacted__"`.

### F2. Panel `WebhookDeliveryPanel.tsx`
Mirror `ReplicationPanel`. `useSectionEditor<AdvancedSectionBody, EventDeliveryConfig>`
with `section:'advanced'`, `dirtyKey:'configuration/advanced/event-delivery'`,
`pick: b => b.event_delivery ?? DEFAULT`, `toPayload` = F4.
UI:
- **Master switch** `enabled`. When off, show an info note that delivery is paused
  (and the outbox still accrues events — they deliver when re-enabled).
- **Endpoints list** — ONE list editing `webhook_urls` (map a legacy single
  `webhook_url` into the list on load, clear it on save → write only `webhook_urls`).
  Add/remove rows with STABLE ids (not array-index keys — admin-editor bug class).
  Validate each is a parseable http(s) URL; inline error per row.
- **Headers** — key/value rows. Value field renders a **masked placeholder** when the
  loaded value is the sentinel ("•••••• (unchanged)"); typing replaces it. A clear
  "edit" affordance to overwrite. Remove-row deletes the header (→ null in payload).
  Validate header NAME (token chars) + non-empty.
- **Advanced disclosure** (collapsed): tick_interval, batch_size, request_timeout,
  max_attempts, retry_base, retry_max, stale_claim_after, delivered_retention,
  delivered_max_rows, prune_batch. Each `Field` with YAML-path breadcrumb + default
  placeholder. Validate humantime durations + numeric ranges client-side (mirror
  `validate_event_delivery`) with inline errors.
- **Live delivery strip** (top): fetch `GET /_/api/admin/event-outbox?limit=1` →
  show `delivery_enabled`/`delivery_active` + pending/failed counts + link to
  `diagnostics/event-outbox`. Read-only.
- **Apply flow**: guarded validation → ApplyDialog → PUT → markApplied; register
  `useApplyHandler` for ⌘S. Discard restores.

### F3. Nav + routing
- `adminNavigation.tsx`: add entry after `sync` (line ~266): path
  `configuration/advanced/event-delivery`, label "Webhook delivery", icon
  `<SendOutlined/>`, section 'advanced', dirtyKey matching.
- `AdminPage.tsx`: import `WebhookDeliveryPanel`; add branch after the `sync` block
  (~730): `if (adminPath === 'configuration/advanced/event-delivery') {...}`.

### F4. `webhookDeliveryPayload.ts` (pure, Node-testable)
- `normalizeEventDelivery(raw): EventDeliveryConfig` — fill defaults, fold legacy
  `webhook_url` into `webhook_urls`.
- `buildEventDeliveryPayload(local, baseline): { ok, body|errors }` — validates
  (URLs, header names, durations, ranges); produces the wire `{ event_delivery: {...} }`
  including:
  - `webhook_urls` always written (legacy `webhook_url` set to `null` to clear it).
  - `webhook_headers`: for each CURRENT header → value (or sentinel passthrough means
    "unchanged" → we send sentinel and backend preserves); for each header in BASELINE
    but REMOVED in current → emit `null` (RFC 7396 delete). THIS is the foot-gun fix.
  - empty headers → send `{}` only if baseline had headers (to clear); else omit.
- Node regression test `webhookDeliveryPayload.test.mjs` covering: removed-header→null,
  unchanged-secret→sentinel, new-secret→value, legacy url fold, duration validation,
  clear-all-headers. (admin-editor bug-class pattern.)

### F5. Usability bugs to NOT ship (the "usability bugs ARE bugs" mandate)
1. Masked header value must be distinguishable from a real value `"••••"` — never let
   the operator accidentally PUT the literal mask string.
2. Removing then re-adding a header with the same name must work (stable-id rows).
3. Enabling delivery with zero endpoints must be blocked with a clear message
   (`is_active` needs ≥1 endpoint) — don't let them save a no-op "enabled" state.
4. Duration fields: accept `30s`/`5m`/`24h`; reject `30`/`5 minutes` with a hint.
5. Dirty-dot + ⌘S + beforeunload must work (via useSectionEditor/useApplyHandler).
6. Discard must restore masked placeholders, not clear them.
7. Apply summary/diff must NOT leak the real token (show sentinel/masked in the diff).

## Gate
- `cargo build && cargo clippy -D warnings && cargo test --lib` + the new section tests.
- `cd demo/s3-browser/ui && npm run build && npm run lint && tsc --noEmit && npx knip`
  + `node webhookDeliveryPayload.test.mjs`.
- `cargo run -- config schema` regenerates clean (no new fields, but verify).
- Local: restart-with-prod-backup, open `/_/admin/configuration/advanced/event-delivery`,
  exercise enable + endpoint + header(secret) + advanced + apply + reload round-trip,
  confirm token not leaked and preserved.
