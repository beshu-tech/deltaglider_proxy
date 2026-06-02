/**
 * Pure payload + validation helpers for the Webhook delivery panel
 * (`event_delivery`). No React / antd imports so the Node regression
 * script can transpile-and-import it directly.
 *
 * ## The two correctness hazards this module owns
 *
 * 1. **Header-delete null.** The server applies RFC 7396 merge-patch to
 *    `webhook_headers`: an absent key is PRESERVED, only an explicit JSON
 *    `null` deletes it. So removing a header in the UI must emit
 *    `{ "<removed-key>": null }` — diffing the edited map against the
 *    BASELINE map. Without this, "delete header" is silently a no-op.
 *
 * 2. **Secret round-trip.** The server masks each header VALUE to
 *    `WEBHOOK_REDACTED_SENTINEL` on GET (keeping the key). A value left as
 *    the sentinel means "unchanged" — we pass it through and the server
 *    restores the real token. A retyped value overwrites. The UI must
 *    never let the operator save the literal sentinel as a real value, and
 *    must never display a real token (it only ever sees the sentinel).
 *
 * 3. **Legacy `webhook_url`.** The backend has both `webhook_url`
 *    (singleton) and `webhook_urls` (list). The UI surfaces ONE endpoint
 *    list bound to `webhook_urls`; on load we fold any legacy single
 *    `webhook_url` into the list, and on save we always write `webhook_urls`
 *    and clear `webhook_url` (→ explicit `null`) so the two never drift.
 */

const WEBHOOK_REDACTED_SENTINEL = '__redacted__';

interface EventDeliveryConfig {
  enabled: boolean;
  webhook_urls: string[];
  webhook_headers: Record<string, string>;
  tick_interval: string;
  batch_size: number;
  request_timeout: string;
  max_attempts: number;
  retry_base: string;
  retry_max: string;
  stale_claim_after: string;
  delivered_retention: string;
  delivered_max_rows: number;
  prune_batch: number;
}

/** Defaults mirror `EventDeliveryConfig::default` in src/config_sections.rs. */
const DEFAULT_EVENT_DELIVERY: EventDeliveryConfig = {
  enabled: false,
  webhook_urls: [],
  webhook_headers: {},
  tick_interval: '10s',
  batch_size: 50,
  request_timeout: '5s',
  max_attempts: 8,
  retry_base: '5s',
  retry_max: '5m',
  stale_claim_after: '60s',
  delivered_retention: '24h',
  delivered_max_rows: 10000,
  prune_batch: 100,
};

/** The wire shape of `event_delivery` as the server GET returns it. */
export interface EventDeliveryWire {
  enabled?: boolean;
  webhook_url?: string | null;
  webhook_urls?: string[];
  webhook_headers?: Record<string, string>;
  tick_interval?: string;
  batch_size?: number;
  request_timeout?: string;
  max_attempts?: number;
  retry_base?: string;
  retry_max?: string;
  stale_claim_after?: string;
  delivered_retention?: string;
  delivered_max_rows?: number;
  prune_batch?: number;
}

export interface AdvancedSectionWebhookBody {
  event_delivery?: EventDeliveryWire;
}

/**
 * Normalize a server `event_delivery` body to the local editing shape:
 * fill defaults for absent fields and FOLD a legacy single `webhook_url`
 * into the `webhook_urls` list (deduped, legacy first).
 */
function normalizeEventDelivery(
  raw: EventDeliveryWire | undefined
): EventDeliveryConfig {
  const d = DEFAULT_EVENT_DELIVERY;
  const ed = raw ?? {};
  const urls: string[] = [];
  if (typeof ed.webhook_url === 'string' && ed.webhook_url.trim()) {
    urls.push(ed.webhook_url.trim());
  }
  for (const u of ed.webhook_urls ?? []) {
    const t = (u ?? '').trim();
    if (t && !urls.includes(t)) urls.push(t);
  }
  return {
    enabled: ed.enabled ?? d.enabled,
    webhook_urls: urls,
    webhook_headers: { ...(ed.webhook_headers ?? {}) },
    tick_interval: ed.tick_interval ?? d.tick_interval,
    batch_size: ed.batch_size ?? d.batch_size,
    request_timeout: ed.request_timeout ?? d.request_timeout,
    max_attempts: ed.max_attempts ?? d.max_attempts,
    retry_base: ed.retry_base ?? d.retry_base,
    retry_max: ed.retry_max ?? d.retry_max,
    stale_claim_after: ed.stale_claim_after ?? d.stale_claim_after,
    delivered_retention: ed.delivered_retention ?? d.delivered_retention,
    delivered_max_rows: ed.delivered_max_rows ?? d.delivered_max_rows,
    prune_batch: ed.prune_batch ?? d.prune_batch,
  };
}

// Compound humantime: one-or-more `<number><unit>` chunks, matching what the
// Rust backend's `humantime::parse_duration` accepts (e.g. `30s`, `5m`, `24h`,
// `1h30m`, `1500ms`). Units cover ns…weeks + common word forms. Kept in sync
// with `humantime` on the Rust side (`src/config_sections.rs::validate_event_delivery`).
const DURATION_RE =
  /^\s*(\d+\s*(ns|us|µs|ms|sec|secs|s|min|mins|m|hr|hrs|h|day|days|d|week|weeks|w)\s*)+$/i;
// http(s) URL with a host. Keep it permissive but reject obvious junk.
const URL_RE = /^https?:\/\/[^\s/$.?#].[^\s]*$/i;
// RFC 7230 token chars for header names.
const HEADER_NAME_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

interface ValidationResult {
  ok: boolean;
  errors: string[];
  /** Wire body for the section PUT, only present when ok. */
  body?: AdvancedSectionWebhookBody;
}

/**
 * Validate the edited config against the BASELINE (the normalized
 * server-loaded value) and produce the section-PUT body.
 *
 * `baseline` is needed to compute header DELETES (keys present at load but
 * removed now → emit `null`).
 */
function buildEventDeliveryPayload(
  local: EventDeliveryConfig,
  baseline: EventDeliveryConfig
): ValidationResult {
  const errors: string[] = [];

  const urls = local.webhook_urls.map((u) => u.trim()).filter((u) => u.length > 0);
  for (const u of urls) {
    if (!URL_RE.test(u)) errors.push(`Endpoint "${u}" is not a valid http(s) URL.`);
  }
  // Usability invariant: enabling delivery with no endpoint is a no-op trap.
  if (local.enabled && urls.length === 0) {
    errors.push('Delivery is enabled but no endpoint is set — add at least one webhook URL or turn delivery off.');
  }

  // Header validation + secret-sentinel guard.
  for (const [name, value] of Object.entries(local.webhook_headers)) {
    if (!HEADER_NAME_RE.test(name)) {
      errors.push(`Header name "${name}" contains invalid characters.`);
    }
    if (value === '') {
      errors.push(`Header "${name}" has an empty value.`);
    }
  }

  const durations: Array<[string, string]> = [
    ['tick_interval', local.tick_interval],
    ['request_timeout', local.request_timeout],
    ['retry_base', local.retry_base],
    ['retry_max', local.retry_max],
    ['stale_claim_after', local.stale_claim_after],
    ['delivered_retention', local.delivered_retention],
  ];
  for (const [field, v] of durations) {
    // delivered_retention accepts "0s" to disable pruning; all parse via the RE.
    if (!DURATION_RE.test(v)) {
      errors.push(`${field} "${v}" is not a duration like 30s, 5m, or 24h.`);
    }
  }

  const numbers: Array<[string, number, number]> = [
    ['batch_size', local.batch_size, 1],
    ['max_attempts', local.max_attempts, 1],
    ['delivered_max_rows', local.delivered_max_rows, 0],
    ['prune_batch', local.prune_batch, 0],
  ];
  for (const [field, v, min] of numbers) {
    if (!Number.isFinite(v) || !Number.isInteger(v) || v < min) {
      errors.push(`${field} must be an integer ≥ ${min}.`);
    }
  }

  if (errors.length > 0) return { ok: false, errors };

  // Build webhook_headers patch: current values + explicit null for removed keys.
  const headerPatch: Record<string, string | null> = {};
  for (const [k, v] of Object.entries(local.webhook_headers)) {
    headerPatch[k] = v; // may be the sentinel (= "unchanged", server restores)
  }
  for (const k of Object.keys(baseline.webhook_headers)) {
    if (!(k in local.webhook_headers)) {
      headerPatch[k] = null; // RFC 7396 delete
    }
  }

  const body: AdvancedSectionWebhookBody = {
    event_delivery: {
      enabled: local.enabled,
      // Always write webhook_urls; clear the legacy singleton so they can't drift.
      webhook_url: null,
      webhook_urls: urls,
      webhook_headers: headerPatch as Record<string, string>,
      tick_interval: local.tick_interval.trim(),
      batch_size: local.batch_size,
      request_timeout: local.request_timeout.trim(),
      max_attempts: local.max_attempts,
      retry_base: local.retry_base.trim(),
      retry_max: local.retry_max.trim(),
      stale_claim_after: local.stale_claim_after.trim(),
      delivered_retention: local.delivered_retention.trim(),
      delivered_max_rows: local.delivered_max_rows,
      prune_batch: local.prune_batch,
    },
  };

  return { ok: true, errors: [], body };
}

// ─────────────────────────────────────────────────────────────────────────
// Form state — the SINGLE source of truth for the panel.
//
// The panel edits this shape directly as its `useSectionEditor` value (no
// parallel mirror), so discard/refresh stay correct for free. Rows carry a
// stable `id` (for React keys) and, for headers, the `origName` + `masked`
// flags needed to render and validate the secret-mask UX. `formFromWire` is
// the editor `pick`; `buildPayloadFromForm` is the validated `toPayload`.
// ─────────────────────────────────────────────────────────────────────────

export interface WebhookUrlRow {
  id: string;
  url: string;
}

export interface WebhookHeaderRow {
  id: string;
  name: string;
  /** Current value. Equals the sentinel while an untouched secret is masked. */
  value: string;
  /** The header name as loaded from the server (empty for a freshly-added
   *  row). Used to detect a rename-while-masked, which would lose the secret. */
  origName: string;
  /** True while `value` is still the server sentinel (untouched secret). */
  masked: boolean;
}

export interface WebhookFormState {
  enabled: boolean;
  urlRows: WebhookUrlRow[];
  headerRows: WebhookHeaderRow[];
  /** Header names present when the form was loaded. Lets `buildPayloadFromForm`
   *  emit RFC 7396 `null` deletes for headers the operator REMOVED (a removed
   *  row is gone from `headerRows`, so its key must be remembered here). Not an
   *  editable field — set once by `formFromWire`, carried through edits. */
  loadedHeaderNames: string[];
  tick_interval: string;
  batch_size: number;
  request_timeout: string;
  max_attempts: number;
  retry_base: string;
  retry_max: string;
  stale_claim_after: string;
  delivered_retention: string;
  delivered_max_rows: number;
  prune_batch: number;
}

// Deterministic id generator INJECTED by the caller so this module stays pure
// (no Math.random / crypto here — the panel passes a per-instance counter).
type IdGen = () => string;

/** Build the editor form state from a server wire body. */
export function formFromWire(
  raw: EventDeliveryWire | undefined,
  nextId: IdGen
): WebhookFormState {
  const cfg = normalizeEventDelivery(raw);
  return {
    enabled: cfg.enabled,
    urlRows: cfg.webhook_urls.map((url) => ({ id: nextId(), url })),
    headerRows: Object.entries(cfg.webhook_headers).map(([name, value]) => ({
      id: nextId(),
      name,
      value,
      origName: name,
      masked: value === WEBHOOK_REDACTED_SENTINEL,
    })),
    loadedHeaderNames: Object.keys(cfg.webhook_headers),
    tick_interval: cfg.tick_interval,
    batch_size: cfg.batch_size,
    request_timeout: cfg.request_timeout,
    max_attempts: cfg.max_attempts,
    retry_base: cfg.retry_base,
    retry_max: cfg.retry_max,
    stale_claim_after: cfg.stale_claim_after,
    delivered_retention: cfg.delivered_retention,
    delivered_max_rows: cfg.delivered_max_rows,
    prune_batch: cfg.prune_batch,
  };
}

/** Flatten form rows back into the validated `EventDeliveryConfig` shape that
 *  `buildEventDeliveryPayload` consumes. Empty (in-progress) rows are dropped;
 *  the baseline is reconstructed from each header's `origName` so removals
 *  produce the right RFC 7396 `null` deletes. */
export function buildPayloadFromForm(form: WebhookFormState): ValidationResult {
  const errors: string[] = [];

  // Rename-while-masked guard (adversarial #2): a masked secret has no value to
  // carry under a NEW key, so the server would drop it. Force re-entry.
  for (const h of form.headerRows) {
    const name = h.name.trim();
    if (h.masked && name && h.origName && name !== h.origName) {
      errors.push(
        `Header "${h.origName}" was renamed to "${name}" without re-entering its value. Re-type the value (it's masked for security) or remove and re-add the header.`
      );
    }
  }

  const local: EventDeliveryConfig = {
    enabled: form.enabled,
    webhook_urls: form.urlRows.map((r) => r.url),
    webhook_headers: {},
    tick_interval: form.tick_interval,
    batch_size: form.batch_size,
    request_timeout: form.request_timeout,
    max_attempts: form.max_attempts,
    retry_base: form.retry_base,
    retry_max: form.retry_max,
    stale_claim_after: form.stale_claim_after,
    delivered_retention: form.delivered_retention,
    delivered_max_rows: form.delivered_max_rows,
    prune_batch: form.prune_batch,
  };
  for (const h of form.headerRows) {
    const name = h.name.trim();
    if (name) local.webhook_headers[name] = h.value;
  }

  // Baseline = the header names present at LOAD time, so headers the operator
  // removed (gone from `headerRows`) still produce an RFC 7396 `null` delete.
  // We only need the KEYS, so any value works.
  const baseline: EventDeliveryConfig = {
    ...local,
    webhook_headers: {},
  };
  for (const name of form.loadedHeaderNames) {
    baseline.webhook_headers[name] = WEBHOOK_REDACTED_SENTINEL;
  }

  const res = buildEventDeliveryPayload(local, baseline);
  if (errors.length > 0) {
    return { ok: false, errors: [...errors, ...res.errors] };
  }
  return res;
}
