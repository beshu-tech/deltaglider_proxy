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
 *
 * 4. **Slack format + bot-token secret.** `format` selects the payload shape:
 *    `raw` (existing JSON envelope) or `slack` (Block Kit message). In `slack`
 *    mode the message lands either via an Incoming Webhook URL (no token) or the
 *    Slack Web API (`slack_bot_token` set, requires `slack_channel`). The bot
 *    token is a SECRET with the SAME mask round-trip as header values: masked to
 *    the sentinel on GET, passed through untouched (= "unchanged", server
 *    restores), overwritten when retyped, cleared to `null` when emptied.
 */

const WEBHOOK_REDACTED_SENTINEL = '__redacted__';

/** Event kinds the backend recognises for `slack_notify_kinds`. Mirrors
 *  `default_slack_notify_kinds` + the valid set in src/config_sections.rs. */
export const SLACK_NOTIFY_KINDS = [
  'ObjectCreated',
  'ObjectDeleted',
  'ObjectCopied',
  'ReplicationObjectCopied',
  'LifecycleTransitioned',
  'LifecycleExpired',
] as const;

type EventDeliveryFormat = 'raw' | 'slack';

/** One bucket/prefix → channel routing rule, wire shape. Mirrors `SlackRoute`
 *  in src/config_sections.rs. `channel` is required; `bucket` absent/empty =
 *  any bucket; `prefix_globs` empty = any key. */
interface SlackRouteWire {
  name?: string | null;
  bucket?: string | null;
  prefix_globs?: string[];
  channel?: string | null;
}

interface SlackRouteNorm {
  name: string;
  bucket: string;
  prefixGlobs: string[];
  channel: string;
}

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
  // Slack
  format: EventDeliveryFormat;
  /** Equals the sentinel while an untouched bot token is masked; '' = unset. */
  slack_bot_token: string;
  slack_channel: string;
  slack_username: string;
  slack_icon_emoji: string;
  slack_include_globs: string[];
  slack_exclude_globs: string[];
  slack_notify_kinds: string[];
  slack_routes: SlackRouteNorm[];
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
  format: 'raw',
  slack_bot_token: '',
  slack_channel: '',
  slack_username: '',
  slack_icon_emoji: '',
  slack_include_globs: [],
  slack_exclude_globs: [],
  slack_notify_kinds: ['ObjectCreated'],
  slack_routes: [],
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
  format?: EventDeliveryFormat;
  slack_bot_token?: string | null;
  slack_channel?: string | null;
  slack_username?: string | null;
  slack_icon_emoji?: string | null;
  slack_include_globs?: string[];
  slack_exclude_globs?: string[];
  slack_notify_kinds?: string[];
  slack_routes?: SlackRouteWire[];
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
    format: ed.format === 'slack' ? 'slack' : 'raw',
    // A null/absent token → unset (''); the sentinel survives as "masked".
    slack_bot_token: ed.slack_bot_token ?? '',
    slack_channel: ed.slack_channel ?? '',
    slack_username: ed.slack_username ?? '',
    slack_icon_emoji: ed.slack_icon_emoji ?? '',
    slack_include_globs: [...(ed.slack_include_globs ?? [])],
    slack_exclude_globs: [...(ed.slack_exclude_globs ?? [])],
    slack_notify_kinds:
      ed.slack_notify_kinds && ed.slack_notify_kinds.length > 0
        ? [...ed.slack_notify_kinds]
        : [...d.slack_notify_kinds],
    slack_routes: (ed.slack_routes ?? []).map((r) => ({
      name: r.name ?? '',
      bucket: r.bucket ?? '',
      prefixGlobs: [...(r.prefix_globs ?? [])],
      channel: r.channel ?? '',
    })),
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

/** A route is "in progress" (safely droppable) only when EVERY field is blank.
 *  Any non-empty field makes it meaningful — so a half-filled route still gets
 *  validated (e.g. a bucket set but no channel → "Route N needs a channel"). */
function routeHasContent(r: SlackRouteNorm): boolean {
  return (
    r.name.trim().length > 0 ||
    r.bucket.trim().length > 0 ||
    r.channel.trim().length > 0 ||
    r.prefixGlobs.some((g) => g.trim().length > 0)
  );
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

  const isSlack = local.format === 'slack';
  const slackToken = local.slack_bot_token.trim();
  // Bot-token mode = a real token typed OR an untouched (masked) token carried
  // over from load. Either way the backend will use the Web API → needs channel.
  const slackBotMode = isSlack && slackToken.length > 0;

  if (!isSlack) {
    // Usability invariant: enabling delivery with no endpoint is a no-op trap.
    if (local.enabled && urls.length === 0) {
      errors.push('Delivery is enabled but no endpoint is set — add at least one webhook URL or turn delivery off.');
    }
  } else {
    // Channel-routing rows that actually carry content (blank rows are dropped).
    const meaningfulRoutes = local.slack_routes.filter((r) => routeHasContent(r));
    if (slackBotMode) {
      // Bot-token mode posts via the Web API. A destination is needed: either
      // the single fallback channel OR at least one route. (Mirrors the backend:
      // it warns only when slack_routes is empty AND slack_channel is unset.)
      if (local.slack_channel.trim().length === 0 && meaningfulRoutes.length === 0) {
        errors.push('Slack bot-token mode needs a channel.');
      }
    } else if (local.enabled && urls.length === 0) {
      // Webhook mode: the hooks.slack.com URL is the only delivery path.
      errors.push('Delivery is enabled but no Slack Incoming Webhook URL is set — add the hooks.slack.com URL or turn delivery off.');
    }
    // Basic glob sanity (backend warns on the rest — don't over-validate).
    for (const g of [...local.slack_include_globs, ...local.slack_exclude_globs]) {
      if (g.trim().length === 0) {
        errors.push('A Slack prefix filter is empty — remove the blank row or fill it in.');
      }
    }
    if (local.slack_notify_kinds.length === 0) {
      errors.push('Pick at least one event kind to post to Slack.');
    }
    // Channel routing (bot-token mode only). Blank rows are dropped; what's left
    // must each carry a channel. Routes outside bot-token mode are a no-op on the
    // backend (Incoming Webhook URLs are 1:1 with a channel) — surface that.
    if (meaningfulRoutes.length > 0 && !slackBotMode) {
      errors.push(
        'Channel routing needs a bot token — Incoming Webhook URLs are each bound to one channel by Slack. Switch to bot-token mode or remove the routes.'
      );
    }
    meaningfulRoutes.forEach((r, i) => {
      if (r.channel.trim().length === 0) {
        errors.push(`Route ${i + 1} needs a channel.`);
      }
    });
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

  // Bot token: sentinel = "unchanged" (pass through, server restores the real
  // token); a real typed value overwrites; empty/whitespace clears to null.
  const tokenTrim = local.slack_bot_token.trim();
  const slackBotToken: string | null =
    local.slack_bot_token === WEBHOOK_REDACTED_SENTINEL
      ? WEBHOOK_REDACTED_SENTINEL
      : tokenTrim.length > 0
        ? tokenTrim
        : null;
  const strOrNull = (v: string): string | null => {
    const t = v.trim();
    return t.length > 0 ? t : null;
  };

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
      // Slack — always emitted so the merge-patch reflects the editor's intent.
      format: local.format,
      slack_bot_token: slackBotToken,
      slack_channel: strOrNull(local.slack_channel),
      slack_username: strOrNull(local.slack_username),
      slack_icon_emoji: strOrNull(local.slack_icon_emoji),
      slack_include_globs: local.slack_include_globs.map((g) => g.trim()).filter((g) => g.length > 0),
      slack_exclude_globs: local.slack_exclude_globs.map((g) => g.trim()).filter((g) => g.length > 0),
      slack_notify_kinds: [...local.slack_notify_kinds],
      // Only routes with a real channel reach the wire; blank/in-progress rows
      // are dropped. name/bucket are omitted when empty (= any bucket; no label).
      slack_routes: local.slack_routes
        .filter((r) => r.channel.trim().length > 0)
        .map((r) => {
          const route: SlackRouteWire = { channel: r.channel.trim() };
          const name = r.name.trim();
          if (name) route.name = name;
          const bucket = r.bucket.trim();
          if (bucket) route.bucket = bucket;
          const globs = r.prefixGlobs.map((g) => g.trim()).filter((g) => g.length > 0);
          if (globs.length > 0) route.prefix_globs = globs;
          return route;
        }),
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

/** A stable-id glob row for the Slack include/exclude prefix filters. Same
 *  id-keyed pattern as {@link WebhookUrlRow} so React keys stay stable across
 *  edits (never array index — see the admin-editor bug class). */
export interface SlackGlobRow {
  id: string;
  glob: string;
}

/** A stable-id channel-routing row: bucket/prefix → channel. Mirrors the wire
 *  `SlackRoute`. Carries a stable `id` (React key — NEVER array index) and a
 *  nested list of stable-id glob rows for the prefix matcher. */
export interface SlackRouteRow {
  id: string;
  name: string;
  bucket: string;
  prefixGlobs: SlackGlobRow[];
  channel: string;
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
  // ── Slack ──
  /** `raw` (existing JSON envelope) or `slack` (Block Kit message). */
  format: EventDeliveryFormat;
  /**
   * UI-only: which Slack sub-mode the operator is editing (Incoming Webhook vs
   * Bot token). The BACKEND mode is derived from whether a token is present, so
   * this is never sent on the wire (`buildPayloadFromForm` ignores it). It lives
   * on the editor value — the single source of truth — so the toggle stays sticky
   * even while the bot-token field is momentarily empty. Initialised from token
   * presence in `formFromWire`. */
  slackPreferBotMode: boolean;
  /** Bot token. Equals the sentinel while an untouched secret is masked. */
  slackBotToken: string;
  /** True while `slackBotToken` is still the server sentinel (untouched). */
  slackBotTokenMasked: boolean;
  slackChannel: string;
  slackUsername: string;
  slackIconEmoji: string;
  slackIncludeRows: SlackGlobRow[];
  slackExcludeRows: SlackGlobRow[];
  slackNotifyKinds: string[];
  /** Per-bucket / per-prefix → channel routes (bot-token mode only). Empty =
   *  the single-channel behavior (`slackChannel` is the only destination). */
  slackRoutes: SlackRouteRow[];
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
    format: cfg.format,
    // Bot mode iff a token (real or masked) is present at load.
    slackPreferBotMode: cfg.slack_bot_token.trim().length > 0,
    slackBotToken: cfg.slack_bot_token,
    slackBotTokenMasked: cfg.slack_bot_token === WEBHOOK_REDACTED_SENTINEL,
    slackChannel: cfg.slack_channel,
    slackUsername: cfg.slack_username,
    slackIconEmoji: cfg.slack_icon_emoji,
    slackIncludeRows: cfg.slack_include_globs.map((glob) => ({ id: nextId(), glob })),
    slackExcludeRows: cfg.slack_exclude_globs.map((glob) => ({ id: nextId(), glob })),
    slackNotifyKinds: [...cfg.slack_notify_kinds],
    slackRoutes: cfg.slack_routes.map((r) => ({
      id: nextId(),
      name: r.name,
      bucket: r.bucket,
      prefixGlobs: r.prefixGlobs.map((glob) => ({ id: nextId(), glob })),
      channel: r.channel,
    })),
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

  // Bot token: a still-masked field carries the sentinel through so the server
  // restores the real token; a retyped (unmasked) field carries the literal.
  const slackBotToken = form.slackBotTokenMasked
    ? WEBHOOK_REDACTED_SENTINEL
    : form.slackBotToken;

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
    format: form.format,
    slack_bot_token: slackBotToken,
    slack_channel: form.slackChannel,
    slack_username: form.slackUsername,
    slack_icon_emoji: form.slackIconEmoji,
    slack_include_globs: form.slackIncludeRows.map((r) => r.glob),
    slack_exclude_globs: form.slackExcludeRows.map((r) => r.glob),
    slack_notify_kinds: [...form.slackNotifyKinds],
    slack_routes: form.slackRoutes.map((r) => ({
      name: r.name,
      bucket: r.bucket,
      prefixGlobs: r.prefixGlobs.map((g) => g.glob),
      channel: r.channel,
    })),
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

// ─────────────────────────────────────────────────────────────────────────
// Preview-only channel resolution (mirrors src/slack_format.rs::resolve_channels).
//
// Best-effort, client-side: a glob match close to `globset` semantics (`**`
// crosses `/`, `*` stays within a path segment). NOT an exact globset port —
// it's a live-preview aid, so bucket-match + simple prefix are what must be
// right. Kept pure so the regression test can exercise it without a server.
// ─────────────────────────────────────────────────────────────────────────

/** Compile one glob to a RegExp. `**` → `.*` (crosses `/`); `*` → `[^/]*`
 *  (within a segment); `?` → `[^/]`. All other regex metachars are escaped. */
function globToRegExp(glob: string): RegExp {
  let re = '';
  for (let i = 0; i < glob.length; i++) {
    const ch = glob[i];
    if (ch === '*') {
      if (glob[i + 1] === '*') {
        re += '.*';
        i++; // consume the second star
      } else {
        re += '[^/]*';
      }
    } else if (ch === '?') {
      re += '[^/]';
    } else if ('.+^${}()|[]\\'.includes(ch)) {
      re += '\\' + ch;
    } else {
      re += ch;
    }
  }
  return new RegExp(`^${re}$`);
}

function globMatchesAny(globs: string[], key: string): boolean {
  for (const g of globs) {
    const trimmed = g.trim();
    if (!trimmed) continue;
    try {
      if (globToRegExp(trimmed).test(key)) return true;
    } catch {
      // An un-compilable glob never matches (the backend warns on it too).
    }
  }
  return false;
}

/** A resolved-channel preview row: which route matched, and its channel. */
interface ResolvedRoute {
  /** Route label (name) if set, else a synthesized description. */
  label: string;
  channel: string;
}

interface ResolveResult {
  /** Channels the sample event would post to, in route order, deduped. */
  matches: ResolvedRoute[];
  /** True when no route matched and we fell back to the single channel. */
  fellBackToChannel: boolean;
  /** The fallback channel used (only meaningful when `fellBackToChannel`). */
  fallbackChannel: string;
}

/**
 * Resolve which channel(s) a SAMPLE `bucket`/`key` event posts to, mirroring
 * the Rust `resolve_channels`: when `routes` is non-empty, fan out to EVERY
 * matching route (bucket absent/empty = any; prefix_globs empty = any key),
 * deduped by channel; otherwise fall back to the single `slackChannel`.
 */
export function resolveSlackChannelsPreview(
  routes: SlackRouteRow[],
  fallbackChannel: string,
  sampleBucket: string,
  sampleKey: string
): ResolveResult {
  const meaningful = routes.filter((r) => r.channel.trim().length > 0);
  if (meaningful.length === 0) {
    const fb = fallbackChannel.trim();
    return { matches: [], fellBackToChannel: fb.length > 0, fallbackChannel: fb };
  }

  const out: ResolvedRoute[] = [];
  const seen = new Set<string>();
  for (const route of meaningful) {
    const bucket = route.bucket.trim();
    if (bucket && bucket !== sampleBucket.trim()) continue;
    const globs = route.prefixGlobs.map((g) => g.glob).filter((g) => g.trim().length > 0);
    if (globs.length > 0 && !globMatchesAny(globs, sampleKey)) continue;
    const channel = route.channel.trim();
    if (!channel || seen.has(channel)) continue;
    seen.add(channel);
    out.push({
      label: route.name.trim() || (bucket ? `bucket ${bucket}` : 'any bucket'),
      channel,
    });
  }

  // Routes EXIST but none matched → the backend posts NOWHERE (resolve_channels
  // only falls back to slack_channel when there are zero routes). Do NOT show
  // the fallback here, or the preview would claim delivery the backend won't do.
  return { matches: out, fellBackToChannel: false, fallbackChannel: '' };
}
