/**
 * Webhook delivery panel — edits `advanced.event_delivery` through the
 * Wave 1 section API (`/api/admin/config/section/advanced`), mirroring the
 * other Advanced sub-panels.
 *
 * Single source of truth: the editor `value` IS the form state (rows + scalars)
 * via `useSectionEditor`'s `pick`/`toPayload`. No parallel mirror — so discard,
 * post-apply refresh, and re-mask all stay correct for free.
 *
 * Secret handling: header VALUES are masked to WEBHOOK_REDACTED_SENTINEL on
 * GET. A masked value shows a placeholder; typing replaces it. On save,
 * untouched (sentinel) values pass through and the server restores the real
 * token; removed headers emit explicit `null` (RFC 7396 delete). Renaming a
 * still-masked header is BLOCKED (the secret can't follow the rename) — the
 * operator must re-type the value or remove/re-add.
 *
 * Usability invariants (usability bugs ARE bugs): enabling with no endpoint is
 * blocked; duration fields hint the format; numeric ranges validated; rows use
 * stable ids; the masked sentinel is never shown or saved as a real value;
 * dirty-dot + ⌘S + discard all work via useSectionEditor.
 */
import { useMemo, useRef } from 'react';
import { useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
  Radio,
  Space,
  Switch,
  Tag,
  Typography,
} from 'antd';
import { ApiOutlined, DeleteOutlined, SendOutlined } from '@ant-design/icons';
import type { SectionApplyResponse } from '../adminApi';
import { fetchEventOutbox } from '../adminApi';
import { useCardStyles, contentColumn, CONTENT_WIDE } from './shared-styles';
import { useColors } from '../ThemeContext';
import { useIsNarrow } from '../useIsNarrow';
import { useSectionEditor } from '../useSectionEditor';
import { useNavigation } from '../NavigationContext';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import ApplyDialog from './ApplyDialog';
import { AdvancedDisclosure } from './ruleEditorFields';
import SlackConnectorCard from './SlackConnectorCard';
import StickyDirtyBar from './StickyDirtyBar';
import RowListEditor from './RowListEditor';
import MaskedSecretInput from './MaskedSecretInput';
import {
  formFromWire,
  buildPayloadFromForm,
  type WebhookFormState,
  type WebhookHeaderRow,
  type WebhookUrlRow,
  type AdvancedSectionWebhookBody,
} from './webhookDeliveryPayload';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

const EMPTY_FORM: WebhookFormState = {
  enabled: false,
  urlRows: [],
  headerRows: [],
  loadedHeaderNames: [],
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
  slackPreferBotMode: false,
  slackBotToken: '',
  slackBotTokenMasked: false,
  slackChannel: '',
  slackUsername: '',
  slackIconEmoji: '',
  slackIncludeRows: [],
  slackExcludeRows: [],
  slackNotifyKinds: ['ObjectCreated'],
  slackRoutes: [],
};

function PanelShell({ children }: { children: React.ReactNode }) {
  return (
    <div
      style={{
        // Wide canvas: the Slack destination zone lays out as two columns
        // (config + pinned preview) and needs the horizontal room.
        ...contentColumn(CONTENT_WIDE),
        display: 'flex',
        flexDirection: 'column',
        gap: 24,
      }}
    >
      {children}
    </div>
  );
}

/** A labeled section divider: a hairline rule with a centered uppercase label,
 *  marking the visual transition from generic config to the connector zone. */
function ConnectorDivider({ label }: { label: string }) {
  const c = useColors();
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 12,
        margin: '4px 2px',
        userSelect: 'none',
      }}
    >
      <div style={{ flex: 1, height: 1, background: c.BORDER }} />
      <span
        style={{
          fontSize: 11,
          fontWeight: 700,
          letterSpacing: 1,
          textTransform: 'uppercase',
          color: c.TEXT_MUTED,
          whiteSpace: 'nowrap',
        }}
      >
        {label}
      </span>
      <div style={{ flex: 1, height: 1, background: c.BORDER }} />
    </div>
  );
}

export default function WebhookDeliveryPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const colors = useColors();
  const nav = useNavigation();
  // Below ~900px the Slack two-column (config + preview) split stacks vertically.
  const isNarrow = useIsNarrow(900);

  // The connector ("destination") card is visually set apart from the generic
  // config above it: a teal accent on the left edge + a faintly tinted surface,
  // so the eye reads "this is where events go" as a distinct zone.
  const connectorCardStyle: React.CSSProperties = {
    ...cardStyle,
    borderLeft: `3px solid ${colors.ACCENT_BLUE}`,
    background: `linear-gradient(${colors.BG_CARD}, ${colors.BG_CARD}) padding-box`,
  };

  // Per-instance row-id counter (no module global) — only used for React keys
  // within this panel instance.
  const seqRef = useRef(0);
  const nextId = useMemo(() => () => `r${seqRef.current++}`, []);

  const {
    value: form,
    setValue: setForm,
    isDirty,
    discard,
    loading,
    error,
    applyOpen,
    applyResponse,
    applying,
    runApply: editorRunApply,
    cancelApply,
    confirmApply,
  } = useSectionEditor<AdvancedSectionWebhookBody, WebhookFormState>({
    section: 'advanced',
    dirtyKey: 'integrations/event-delivery',
    initial: EMPTY_FORM,
    onSessionExpired,
    noun: 'webhook delivery',
    pick: (body) => formFromWire(body.event_delivery, nextId),
    // Guarded runApply below blocks on validation failure, so this only runs
    // for a valid form; `{}` is the unreachable type-non-null fallback.
    toPayload: (v) => {
      const res = buildPayloadFromForm(v);
      return res.ok ? (res.body as AdvancedSectionWebhookBody) : {};
    },
  });

  // Live delivery status strip (best-effort, read-only).
  const [outbox, setOutbox] = useState<{
    pending: number;
    failed: number;
    enabled: boolean;
    active: boolean;
  } | null>(null);
  useEffect(() => {
    let alive = true;
    fetchEventOutbox(1)
      .then((r) => {
        if (!alive) return;
        setOutbox({
          pending: r.counts.pending,
          failed: r.counts.failed,
          enabled: r.delivery_enabled,
          active: r.delivery_active,
        });
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

  // ── Row + field mutators (edit the editor value directly) ──
  // Functional updater: a burst of edits within one tick always builds on the
  // freshest state (`setForm` from useSectionEditor supports `prev => next`),
  // never a closed-over render snapshot. `setField` accepts a flat patch OR a
  // `prev => patch` fn so row mutators can splice against the latest rows.
  const setField = (
    patch: Partial<WebhookFormState> | ((prev: WebhookFormState) => Partial<WebhookFormState>),
  ) => setForm((prev) => ({ ...prev, ...(typeof patch === 'function' ? patch(prev) : patch) }));

  const updateUrl = (id: string, url: string) =>
    setField((prev) => ({ urlRows: prev.urlRows.map((r) => (r.id === id ? { ...r, url } : r)) }));
  const addUrl = () =>
    setField((prev) => ({ urlRows: [...prev.urlRows, { id: nextId(), url: '' } as WebhookUrlRow] }));
  const removeUrl = (id: string) =>
    setField((prev) => ({ urlRows: prev.urlRows.filter((r) => r.id !== id) }));

  // Header rows are edited inline via RowListEditor in the raw-webhook block;
  // the value field's MaskedSecretInput emits `{ value, masked: false }` so a
  // retyped secret unmasks. (urlRows mutators above are still shared with the
  // Slack connector card, which reuses the webhook_urls editor.)

  // Live, non-blocking validation preview.
  const liveErrors = useMemo(() => {
    if (!isDirty) return [];
    const res = buildPayloadFromForm(form);
    return res.ok ? [] : res.errors;
  }, [form, isDirty]);

  const runApply = () => {
    const res = buildPayloadFromForm(form);
    if (!res.ok) return; // errors already shown via liveErrors
    editorRunApply();
  };

  if (error) return <Alert type="error" showIcon message="Failed to load" description={error} />;
  if (loading)
    return (
      <PanelShell>
        <Text type="secondary">Loading…</Text>
      </PanelShell>
    );

  return (
    <PanelShell>
      {/* Raw-mode errors surface here, compactly; Slack-mode errors render
          inline in the connector card. The sticky bar below carries the count +
          actions so this block staying mounted (min-height) avoids layout shift. */}
      {form.format === 'raw' && liveErrors.length > 0 && (
        <Alert
          type="error"
          showIcon
          banner
          message={
            <span style={{ fontSize: 13 }}>
              {liveErrors.join(' · ')}
            </span>
          }
          style={{ borderRadius: 8 }}
        />
      )}

      {outbox && (
        <Alert
          type={outbox.active ? 'success' : 'info'}
          showIcon
          message={
            <Space size="middle" wrap>
              <span>
                Delivery:{' '}
                <Tag color={outbox.active ? 'green' : outbox.enabled ? 'orange' : 'default'}>
                  {outbox.active ? 'Active' : outbox.enabled ? 'Enabled (no endpoint)' : 'Disabled'}
                </Tag>
              </span>
              <span>{outbox.pending} pending</span>
              <span>{outbox.failed} failed</span>
              <Button
                type="link"
                size="small"
                style={{ padding: 0 }}
                onClick={() => nav.navigate('admin/integrations/event-outbox')}
              >
                View event outbox →
              </Button>
            </Space>
          }
        />
      )}

      {/* ── 1. GENERIC DELIVERY CONFIG (all format-agnostic settings together) ── */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<SendOutlined />}
          title="Event delivery"
          description="Deliver durable object events (create/delete/copy) downstream. The outbox accrues events even while disabled — they deliver once you enable + configure a destination below."
        />
        <div>
          <FormField label="Enable delivery" yamlPath="advanced.event_delivery.enabled">
            <Switch checked={form.enabled} onChange={(v) => setField({ enabled: v })} />
          </FormField>

          <FormField
            label="Payload format"
            yamlPath="advanced.event_delivery.format"
            helpText="Raw posts the deltaglider.event.v1 JSON envelope to your endpoints. Slack formats each event as a chat message."
          >
            <Radio.Group
              value={form.format}
              onChange={(e) => setField({ format: e.target.value as 'raw' | 'slack' })}
              style={{ display: 'flex', gap: 0 }}
            >
              <Radio.Button value="raw" style={{ fontSize: 13 }} title="Raw JSON webhook payload">
                Raw webhook
              </Radio.Button>
              <Radio.Button value="slack" style={{ fontSize: 13 }} title="Format events as Slack messages">
                Slack
              </Radio.Button>
            </Radio.Group>
          </FormField>
        </div>

        {/* Generic delivery tuning lives WITH the rest of the generic config —
            it's format-agnostic (applies to raw + Slack alike). */}
        <div style={{ marginTop: 8 }}>
          <AdvancedDisclosure title="Delivery tuning (retry, retention, batching)">
            <DurationField
              label="Tick interval"
              yamlPath="advanced.event_delivery.tick_interval"
              help="How often the dispatcher wakes to deliver due events."
              value={form.tick_interval}
              placeholder="10s"
              onChange={(v) => setField({ tick_interval: v })}
              inputRadius={inputRadius}
            />
            <NumberField
              label="Batch size"
              yamlPath="advanced.event_delivery.batch_size"
              help="Max events claimed per tick (clamped 1–500)."
              value={form.batch_size}
              min={1}
              max={500}
              onChange={(v) => setField({ batch_size: v })}
            />
            <DurationField
              label="Request timeout"
              yamlPath="advanced.event_delivery.request_timeout"
              help="Per-endpoint HTTP timeout."
              value={form.request_timeout}
              placeholder="5s"
              onChange={(v) => setField({ request_timeout: v })}
              inputRadius={inputRadius}
            />
            <NumberField
              label="Max attempts"
              yamlPath="advanced.event_delivery.max_attempts"
              help="Attempts before an event becomes permanently failed."
              value={form.max_attempts}
              min={1}
              onChange={(v) => setField({ max_attempts: v })}
            />
            <DurationField
              label="Retry base"
              yamlPath="advanced.event_delivery.retry_base"
              help="Initial retry delay; exponential backoff doubles it per attempt."
              value={form.retry_base}
              placeholder="5s"
              onChange={(v) => setField({ retry_base: v })}
              inputRadius={inputRadius}
            />
            <DurationField
              label="Retry max"
              yamlPath="advanced.event_delivery.retry_max"
              help="Ceiling for the backoff delay."
              value={form.retry_max}
              placeholder="5m"
              onChange={(v) => setField({ retry_max: v })}
              inputRadius={inputRadius}
            />
            <DurationField
              label="Stale claim after"
              yamlPath="advanced.event_delivery.stale_claim_after"
              help="In-progress claims older than this are reclaimable (crash recovery)."
              value={form.stale_claim_after}
              placeholder="60s"
              onChange={(v) => setField({ stale_claim_after: v })}
              inputRadius={inputRadius}
            />
            <DurationField
              label="Delivered retention"
              yamlPath="advanced.event_delivery.delivered_retention"
              help="Delivered rows older than this are pruned. 0s keeps them until manually pruned."
              value={form.delivered_retention}
              placeholder="24h"
              onChange={(v) => setField({ delivered_retention: v })}
              inputRadius={inputRadius}
            />
            <NumberField
              label="Delivered max rows"
              yamlPath="advanced.event_delivery.delivered_max_rows"
              help="Cap on retained delivered rows (pending/failed never pruned by this)."
              value={form.delivered_max_rows}
              min={0}
              onChange={(v) => setField({ delivered_max_rows: v })}
            />
            <NumberField
              label="Prune batch"
              yamlPath="advanced.event_delivery.prune_batch"
              help="Max delivered rows pruned per tick."
              value={form.prune_batch}
              min={0}
              onChange={(v) => setField({ prune_batch: v })}
            />
          </AdvancedDisclosure>
        </div>
      </div>

      {/* ── 2. DESTINATION ZONE — labeled divider sets the connector apart ── */}
      <ConnectorDivider label={form.format === 'slack' ? 'Slack destination' : 'Webhook destination'} />

      {/* The connector card carries an accent left-border so it reads as a
          distinct "where does it go" zone, separate from the generic config. */}
      <div style={connectorCardStyle}>
        {form.format === 'raw' ? (
          <>
            <SectionHeader
              icon={<ApiOutlined />}
              title="Raw webhook destination"
              description="POST the deltaglider.event.v1 JSON envelope to one or more HTTP endpoints with optional static headers."
            />
            <div>
              <FormField
                label="Endpoints"
                yamlPath="advanced.event_delivery.webhook_urls"
                helpText="HTTP(S) URLs that receive the payload. An event is marked delivered only after every endpoint returns 2xx."
              >
                <RowListEditor<WebhookUrlRow>
                  items={form.urlRows}
                  onChange={(urlRows) => setField({ urlRows })}
                  newItem={() => ({ id: nextId(), url: '' })}
                  addLabel="Add endpoint"
                  emptyHint={
                    <Text type="secondary" style={{ fontSize: 13 }}>
                      No endpoints. Add one to start delivering.
                    </Text>
                  }
                  renderRow={(row, update, remove) => (
                    <Space.Compact style={{ width: '100%' }}>
                      <Input
                        value={row.url}
                        onChange={(e) => update({ url: e.target.value })}
                        placeholder="https://hooks.example.com/deltaglider"
                        style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 14 }}
                      />
                      <Button
                        icon={<DeleteOutlined />}
                        onClick={remove}
                        title="Remove endpoint"
                      />
                    </Space.Compact>
                  )}
                />
              </FormField>

              <FormField
                label="Headers"
                yamlPath="advanced.event_delivery.webhook_headers"
                helpText="Static headers sent with every request — e.g. an Authorization bearer token. Values are stored encrypted and shown masked; leave a masked value untouched to keep it."
              >
                <RowListEditor<WebhookHeaderRow>
                  items={form.headerRows}
                  onChange={(headerRows) => setField({ headerRows })}
                  newItem={() => ({
                    id: nextId(),
                    name: '',
                    value: '',
                    origName: '',
                    masked: false,
                  })}
                  addLabel="Add header"
                  emptyHint={
                    <Text type="secondary" style={{ fontSize: 13 }}>
                      No headers.
                    </Text>
                  }
                  renderRow={(row, update, remove) => (
                    <Space.Compact style={{ width: '100%' }}>
                      <Input
                        value={row.name}
                        onChange={(e) => update({ name: e.target.value })}
                        placeholder="Authorization"
                        style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 14, width: '40%' }}
                      />
                      <MaskedSecretInput
                        mode="sentinel"
                        reveal
                        value={row.value}
                        masked={row.masked}
                        // Typing unmasks: it's now a real, operator-entered value.
                        onChange={(value) => update({ value, masked: false })}
                        style={{ ...inputRadius, fontSize: 14, flex: 1 }}
                      />
                      <Button
                        icon={<DeleteOutlined />}
                        onClick={remove}
                        title="Remove header"
                      />
                    </Space.Compact>
                  )}
                />
              </FormField>
            </div>
          </>
        ) : (
          <SlackConnectorCard
            form={form}
            setField={setField}
            nextId={nextId}
            errors={liveErrors}
            inputRadius={inputRadius}
            updateUrl={updateUrl}
            addUrl={addUrl}
            removeUrl={removeUrl}
            isNarrow={isNarrow}
          />
        )}
      </div>

      <ApplyDialog
        open={applyOpen}
        section="advanced"
        response={applyResponse as SectionApplyResponse | null}
        onApply={confirmApply}
        onCancel={cancelApply}
        loading={applying}
      />

      {/* Slim floating action bar — pins to the bottom of the scroll viewport,
          never displaces content above it. */}
      <StickyDirtyBar
        visible={isDirty}
        applying={applying}
        errorCount={liveErrors.length}
        onDiscard={discard}
        onApply={runApply}
      />
    </PanelShell>
  );
}

function DurationField(props: {
  label: string;
  yamlPath: string;
  help: string;
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
  inputRadius: React.CSSProperties;
}) {
  return (
    <FormField
      label={props.label}
      yamlPath={props.yamlPath}
      helpText={`${props.help} Format: 30s, 5m, 24h (compound like 1h30m ok).`}
    >
      <Input
        value={props.value}
        onChange={(e) => props.onChange(e.target.value)}
        placeholder={props.placeholder}
        style={{ ...props.inputRadius, fontFamily: 'var(--font-mono)', fontSize: 14, maxWidth: 200 }}
      />
    </FormField>
  );
}

function NumberField(props: {
  label: string;
  yamlPath: string;
  help: string;
  value: number;
  min: number;
  max?: number;
  onChange: (v: number) => void;
}) {
  return (
    <FormField label={props.label} yamlPath={props.yamlPath} helpText={props.help}>
      <InputNumber
        value={props.value}
        min={props.min}
        max={props.max}
        onChange={(v) => props.onChange(typeof v === 'number' ? v : props.min)}
        style={{ maxWidth: 200 }}
      />
    </FormField>
  );
}
