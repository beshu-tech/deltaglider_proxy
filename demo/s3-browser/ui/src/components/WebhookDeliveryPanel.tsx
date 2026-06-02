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
  Space,
  Switch,
  Tag,
  Typography,
} from 'antd';
import { DeleteOutlined, PlusOutlined, SendOutlined } from '@ant-design/icons';
import type { SectionApplyResponse } from '../adminApi';
import { fetchEventOutbox } from '../adminApi';
import { useCardStyles } from './shared-styles';
import { useSectionEditor } from '../useSectionEditor';
import { useNavigation } from '../NavigationContext';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import ApplyDialog from './ApplyDialog';
import { AdvancedDisclosure } from './ruleEditorFields';
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
};

function PanelShell({ children }: { children: React.ReactNode }) {
  return (
    <div
      style={{
        maxWidth: 740,
        margin: '0 auto',
        padding: 'clamp(16px, 3vw, 24px)',
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      {children}
    </div>
  );
}

export default function WebhookDeliveryPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const nav = useNavigation();

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
    dirtyKey: 'configuration/advanced/event-delivery',
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
  const setField = (patch: Partial<WebhookFormState>) => setForm({ ...form, ...patch });

  const updateUrl = (id: string, url: string) =>
    setField({ urlRows: form.urlRows.map((r) => (r.id === id ? { ...r, url } : r)) });
  const addUrl = () =>
    setField({ urlRows: [...form.urlRows, { id: nextId(), url: '' } as WebhookUrlRow] });
  const removeUrl = (id: string) =>
    setField({ urlRows: form.urlRows.filter((r) => r.id !== id) });

  const updateHeader = (id: string, patch: Partial<WebhookHeaderRow>) =>
    setField({
      headerRows: form.headerRows.map((r) =>
        r.id === id
          ? {
              ...r,
              ...patch,
              // Editing the VALUE unmasks it (now a real, operator-typed value).
              masked: patch.value !== undefined ? false : r.masked,
            }
          : r
      ),
    });
  const addHeader = () =>
    setField({
      headerRows: [
        ...form.headerRows,
        { id: nextId(), name: '', value: '', origName: '', masked: false } as WebhookHeaderRow,
      ],
    });
  const removeHeader = (id: string) =>
    setField({ headerRows: form.headerRows.filter((r) => r.id !== id) });

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
      {isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved changes to webhook delivery"
          description="Review the diff in the Apply dialog before persisting. Changes apply live — no restart."
          action={
            <Space>
              <Button size="small" onClick={discard} disabled={applying}>
                Discard
              </Button>
              <Button
                type="primary"
                size="small"
                onClick={runApply}
                disabled={applying || liveErrors.length > 0}
                loading={applying}
              >
                Apply
              </Button>
            </Space>
          }
        />
      )}

      {liveErrors.length > 0 && (
        <Alert
          type="error"
          showIcon
          message="Fix these before applying"
          description={
            <ul style={{ margin: 0, paddingLeft: 18 }}>
              {liveErrors.map((e, i) => (
                <li key={i}>{e}</li>
              ))}
            </ul>
          }
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
                onClick={() => nav.navigate('admin/diagnostics/event-outbox')}
              >
                View event outbox →
              </Button>
            </Space>
          }
        />
      )}

      {/* Master switch + endpoints */}
      <div style={cardStyle}>
        <SectionHeader
          icon={<SendOutlined />}
          title="Webhook delivery"
          description="Deliver durable object events (create/delete/copy) to HTTP endpoints. The outbox accrues events even while disabled — they deliver once you enable + add an endpoint."
        />
        <div style={{ marginTop: 16 }}>
          <FormField label="Enable delivery" yamlPath="advanced.event_delivery.enabled">
            <Switch checked={form.enabled} onChange={(v) => setField({ enabled: v })} />
          </FormField>

          <FormField
            label="Endpoints"
            yamlPath="advanced.event_delivery.webhook_urls"
            helpText="HTTP(S) URLs that receive a deltaglider.event.v1 JSON payload. An event is marked delivered only after every endpoint returns 2xx."
          >
            <Space direction="vertical" style={{ width: '100%' }}>
              {form.urlRows.length === 0 && (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  No endpoints. Add one to start delivering.
                </Text>
              )}
              {form.urlRows.map((row) => (
                <Space.Compact key={row.id} style={{ width: '100%' }}>
                  <Input
                    value={row.url}
                    onChange={(e) => updateUrl(row.id, e.target.value)}
                    placeholder="https://hooks.example.com/deltaglider"
                    style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
                  />
                  <Button
                    icon={<DeleteOutlined />}
                    onClick={() => removeUrl(row.id)}
                    title="Remove endpoint"
                  />
                </Space.Compact>
              ))}
              <Button icon={<PlusOutlined />} onClick={addUrl} size="small">
                Add endpoint
              </Button>
            </Space>
          </FormField>

          <FormField
            label="Headers"
            yamlPath="advanced.event_delivery.webhook_headers"
            helpText="Static headers sent with every request — e.g. an Authorization bearer token. Values are stored encrypted and shown masked; leave a masked value untouched to keep it."
          >
            <Space direction="vertical" style={{ width: '100%' }}>
              {form.headerRows.length === 0 && (
                <Text type="secondary" style={{ fontSize: 12 }}>
                  No headers.
                </Text>
              )}
              {form.headerRows.map((row) => (
                <Space.Compact key={row.id} style={{ width: '100%' }}>
                  <Input
                    value={row.name}
                    onChange={(e) => updateHeader(row.id, { name: e.target.value })}
                    placeholder="Authorization"
                    style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, width: '40%' }}
                  />
                  <Input
                    value={row.masked ? '' : row.value}
                    onChange={(e) => updateHeader(row.id, { value: e.target.value })}
                    placeholder={row.masked ? '•••••••• (unchanged — type to replace)' : 'Bearer …'}
                    style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
                  />
                  <Button
                    icon={<DeleteOutlined />}
                    onClick={() => removeHeader(row.id)}
                    title="Remove header"
                  />
                </Space.Compact>
              ))}
              <Button icon={<PlusOutlined />} onClick={addHeader} size="small">
                Add header
              </Button>
            </Space>
          </FormField>
        </div>
      </div>

      {/* Advanced tuning */}
      <div style={cardStyle}>
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

      <ApplyDialog
        open={applyOpen}
        section="advanced"
        response={applyResponse as SectionApplyResponse | null}
        onApply={confirmApply}
        onCancel={cancelApply}
        loading={applying}
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
        style={{ ...props.inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 160 }}
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
        style={{ maxWidth: 160 }}
      />
    </FormField>
  );
}
