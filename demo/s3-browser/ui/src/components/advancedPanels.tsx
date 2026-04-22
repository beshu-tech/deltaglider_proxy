/**
 * Advanced sub-section panels (Wave 7 of the admin UI revamp).
 *
 * Five lightweight panels, one per sub-path under
 * `/configuration/advanced/...`. Each edits a single scope of
 * `advanced.*` fields through the Wave 1 section API (`/api/admin/
 * config/section/advanced`), except Limits which is a read-only
 * env-var status page.
 *
 * | Panel               | Fields                                      | Hot-reload? |
 * |---------------------|---------------------------------------------|-------------|
 * | ListenerTlsPanel    | listen_addr, tls { enabled, cert, key }     | Restart     |
 * | CachesPanel         | cache_size_mb, metadata_cache_mb,           | Mixed       |
 * |                     | codec_concurrency, blocking_threads         |             |
 * | LimitsPanel         | request_timeout_secs, max_concurrent,       | Env-var     |
 * |                     | max_multipart_uploads                       |  (restart)  |
 * | LoggingPanel        | log_level (EnvFilter)                       | Hot         |
 * | ConfigDbSyncPanel   | config_sync_bucket                          | Restart     |
 *
 * Restart-required fields get a compact amber chip next to the
 * input's label. The actual "applied but needs restart" surfaces
 * through the `requires_restart` flag on the section PUT response —
 * ApplyDialog renders it as a blue banner.
 */
import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
  Radio,
  Space,
  Switch,
  Typography,
  message,
} from 'antd';
import {
  CloudServerOutlined,
  DatabaseOutlined,
  CloudOutlined,
  ControlOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import type { AdminConfig, SectionApplyResponse } from '../adminApi';
import {
  getAdminConfig,
  getSection,
  putSection,
  validateSection,
} from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import { useDirtySection, useApplyHandler } from '../useDirtySection';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import ApplyDialog from './ApplyDialog';

const { Text } = Typography;

// ───────────────────────────────────────────────────────────
// Shared primitives
// ───────────────────────────────────────────────────────────

interface PanelProps {
  onSessionExpired?: () => void;
}

// ───────────────────────────────────────────────────────────
// Per-panel initial value objects — hoisted to module scope so
// they get allocated once, not once per panel render. Each panel
// passes its INITIAL into `useAdvancedSubset(initial, ...)` →
// `useDirtySection(section, initial)` → `useState(initial)`.
// useState only honors the first value, so a fresh literal each
// render was silently discarded, but it was misleading (suggested
// state depended on `initial` while actually being frozen at
// mount). It also needlessly allocated 4 small objects per render
// across the 4 panels that use this pattern.
// ───────────────────────────────────────────────────────────

const LISTENER_INITIAL: Pick<AdvancedSectionBody, 'listen_addr' | 'tls'> = {
  listen_addr: undefined,
  tls: undefined,
};

const CACHES_INITIAL: Pick<
  AdvancedSectionBody,
  'cache_size_mb' | 'metadata_cache_mb' | 'codec_concurrency' | 'blocking_threads'
> = {
  cache_size_mb: undefined,
  metadata_cache_mb: undefined,
  codec_concurrency: undefined,
  blocking_threads: undefined,
};

const LOG_INITIAL: Pick<AdvancedSectionBody, 'log_level'> = {
  log_level: undefined,
};

const SYNC_INITIAL: Pick<AdvancedSectionBody, 'config_sync_bucket'> = {
  config_sync_bucket: undefined,
};

/** The AdvancedSection wire shape — every field is optional (the
 *  server omits defaults on GET). */
interface AdvancedSectionBody {
  listen_addr?: string;
  cache_size_mb?: number;
  metadata_cache_mb?: number;
  codec_concurrency?: number;
  blocking_threads?: number;
  log_level?: string;
  config_sync_bucket?: string;
  tls?: {
    enabled?: boolean;
    cert_path?: string | null;
    key_path?: string | null;
  };
}

/** Small amber chip next to a label indicating that the field
 *  requires a server restart to take effect. */
function RestartChip({ reason }: { reason: string }) {
  const { ACCENT_AMBER } = useColors();
  return (
    <span
      title={reason}
      style={{
        display: 'inline-block',
        fontSize: 9,
        fontWeight: 700,
        letterSpacing: 0.5,
        textTransform: 'uppercase',
        padding: '1px 6px',
        borderRadius: 10,
        background: `${ACCENT_AMBER}20`,
        color: ACCENT_AMBER,
        marginLeft: 8,
        cursor: 'help',
        verticalAlign: 'middle',
      }}
    >
      Restart required
    </span>
  );
}

/**
 * Hook: fetch + track dirty state for the `advanced` section,
 * scoped to a subset of fields. Each sub-panel passes a `pick`
 * function that selects the fields it cares about; the hook
 * returns the full subset state + the usual dirty-state machinery.
 *
 * PUT sends ONLY the fields the panel owns (RFC 7396 merge patch
 * preserves everything else), so sibling Advanced panels can't
 * clobber each other.
 */
function useAdvancedSubset<T extends Partial<AdvancedSectionBody>>(
  initial: T,
  onSessionExpired?: () => void
) {
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const { value, isDirty, setValue, discard, markApplied, resetWith } =
    useDirtySection<T>('advanced', initial);

  // Apply-dialog state.
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(null);
  const [pendingBody, setPendingBody] = useState<T | null>(null);
  const [applying, setApplying] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const body = await getSection<AdvancedSectionBody>('advanced');
      // Pick just the subset this panel owns. Unknown keys stay
      // absent from our state but the server keeps them when we
      // PUT our subset back (merge-patch).
      const subset: Partial<T> = {};
      for (const k of Object.keys(initial) as Array<keyof T>) {
        const v = (body as Record<string, unknown>)[k as string];
        if (v !== undefined) {
          (subset as Record<string, unknown>)[k as string] = v;
        }
      }
      resetWith({ ...initial, ...subset });
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(
        `Failed to load advanced section: ${e instanceof Error ? e.message : 'unknown'}`
      );
    } finally {
      setLoading(false);
    }
    // Only run on mount / prop change; `initial` + `resetWith` are
    // expected to be stable.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onSessionExpired]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  /** Kick the plan -> diff -> apply dialog. Snapshots the body at
   *  validate time so a later edit under the dialog can't drift. */
  const runApply = async () => {
    const snapshot = { ...value };
    try {
      const resp = await validateSection<T>('advanced', snapshot);
      setApplyResponse(resp);
      setPendingBody(snapshot);
      setApplyOpen(true);
    } catch (e) {
      message.error(
        `Validate failed: ${e instanceof Error ? e.message : 'unknown'}`
      );
    }
  };

  const cancelApply = () => {
    setApplyOpen(false);
    setPendingBody(null);
  };

  const confirmApply = async () => {
    if (!pendingBody) return;
    setApplying(true);
    try {
      const resp = await putSection<T>('advanced', pendingBody);
      if (!resp.ok) {
        message.error(resp.error || 'Apply failed');
        return;
      }
      message.success(
        resp.persisted_path
          ? `Applied + persisted to ${resp.persisted_path}`
          : 'Applied'
      );
      markApplied();
      setApplyOpen(false);
      setPendingBody(null);
      void refresh();
    } catch (e) {
      message.error(`Apply failed: ${e instanceof Error ? e.message : 'unknown'}`);
      setApplyOpen(false);
      setPendingBody(null);
      void refresh();
    } finally {
      setApplying(false);
    }
  };

  // ⌘S wiring: any Advanced sub-panel inherits ⌘S-to-Apply for free.
  // Each panel mounts its own useAdvancedSubset — only the visible
  // panel's handler sits on top of the register stack, which is
  // exactly what requestApplyCurrent dispatches to.
  useApplyHandler('advanced', runApply, isDirty);

  return {
    value,
    isDirty,
    setValue,
    discard,
    loading,
    error,
    applyOpen,
    applyResponse,
    applying,
    runApply,
    cancelApply,
    confirmApply,
  };
}

/** Render the dirty-state banner + ApplyDialog pair. Every Advanced
 *  sub-panel uses this same tail. */
function AdvancedApplyRail(props: {
  isDirty: boolean;
  applying: boolean;
  onDiscard: () => void;
  onApply: () => void;
  applyOpen: boolean;
  applyResponse: SectionApplyResponse | null;
  cancelApply: () => void;
  confirmApply: () => void;
}) {
  return (
    <>
      {props.isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved changes to this section"
          description="Review the diff in the Apply dialog before persisting."
          action={
            <Space>
              <Button size="small" onClick={props.onDiscard} disabled={props.applying}>
                Discard
              </Button>
              <Button
                type="primary"
                size="small"
                onClick={props.onApply}
                disabled={props.applying}
                loading={props.applying}
              >
                Apply
              </Button>
            </Space>
          }
        />
      )}
      <ApplyDialog
        open={props.applyOpen}
        section="advanced"
        response={props.applyResponse}
        onApply={props.confirmApply}
        onCancel={props.cancelApply}
        loading={props.applying}
      />
    </>
  );
}

function PanelShell(props: { children: React.ReactNode }) {
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
      {props.children}
    </div>
  );
}

// ───────────────────────────────────────────────────────────
// ListenerTlsPanel  (advanced.listen_addr + advanced.tls.*)
// ───────────────────────────────────────────────────────────

export function ListenerTlsPanel({ onSessionExpired }: PanelProps) {
  const { cardStyle, inputRadius } = useCardStyles();
  const subset = useAdvancedSubset(LISTENER_INITIAL, onSessionExpired);
  const { value, setValue, isDirty, discard, loading, error } = subset;

  if (error) return <Alert type="error" showIcon message="Failed to load" description={error} />;
  if (loading) return <PanelShell><Text type="secondary">Loading...</Text></PanelShell>;

  const tls = value.tls ?? {};
  const tlsEnabled = tls.enabled === true;

  const setListenAddr = (v: string) =>
    setValue({ ...value, listen_addr: v || undefined });
  const setTls = (patch: Partial<NonNullable<AdvancedSectionBody['tls']>>) =>
    setValue({ ...value, tls: { ...tls, ...patch } });

  return (
    <PanelShell>
      <AdvancedApplyRail
        isDirty={isDirty}
        applying={subset.applying}
        onDiscard={discard}
        onApply={subset.runApply}
        applyOpen={subset.applyOpen}
        applyResponse={subset.applyResponse}
        cancelApply={subset.cancelApply}
        confirmApply={subset.confirmApply}
      />

      <div style={cardStyle}>
        <SectionHeader
          icon={<CloudServerOutlined />}
          title="HTTP listener"
          description="Where the proxy binds for incoming S3 traffic."
        />
        <div style={{ marginTop: 16 }}>
          <FormField
            label={
              <>
                Listen address
                <RestartChip reason="Changing listen_addr requires the HTTP socket to re-bind — needs a server restart." />
              </>
            }
            yamlPath="advanced.listen_addr"
            helpText="host:port. Default 0.0.0.0:9000."
            examples={['0.0.0.0:9000', '127.0.0.1:9001']}
            onExampleClick={(v) => setListenAddr(String(v))}
          >
            <Input
              value={value.listen_addr ?? ''}
              onChange={(e) => setListenAddr(e.target.value)}
              placeholder="0.0.0.0:9000"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
            />
          </FormField>
        </div>
      </div>

      <div style={cardStyle}>
        <SectionHeader
          icon={<CloudServerOutlined />}
          title="TLS"
          description="Optional HTTPS for the listener. When disabled, the proxy speaks plain HTTP — put it behind a reverse proxy that terminates TLS for you."
        />
        <div style={{ marginTop: 16 }}>
          <FormField label="Enable TLS" yamlPath="advanced.tls.enabled">
            <Switch
              checked={tlsEnabled}
              onChange={(v) => setTls({ enabled: v })}
            />
          </FormField>
          {tlsEnabled && (
            <>
              <FormField
                label={
                  <>
                    Certificate path
                    <RestartChip reason="TLS certs load at startup." />
                  </>
                }
                yamlPath="advanced.tls.cert_path"
                helpText="Absolute path to the PEM-encoded certificate."
              >
                <Input
                  value={tls.cert_path ?? ''}
                  onChange={(e) => setTls({ cert_path: e.target.value || null })}
                  placeholder="/etc/ssl/certs/deltaglider.pem"
                  style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
                />
              </FormField>
              <FormField
                label={
                  <>
                    Private key path
                    <RestartChip reason="TLS keys load at startup." />
                  </>
                }
                yamlPath="advanced.tls.key_path"
                helpText="Absolute path to the PEM-encoded private key."
              >
                <Input
                  value={tls.key_path ?? ''}
                  onChange={(e) => setTls({ key_path: e.target.value || null })}
                  placeholder="/etc/ssl/private/deltaglider.key"
                  style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
                />
              </FormField>
            </>
          )}
        </div>
      </div>
    </PanelShell>
  );
}

// ───────────────────────────────────────────────────────────
// CachesPanel  (cache_size_mb, metadata_cache_mb, codec_concurrency, blocking_threads)
// ───────────────────────────────────────────────────────────

export function CachesPanel({ onSessionExpired }: PanelProps) {
  const { cardStyle, inputRadius } = useCardStyles();
  const subset = useAdvancedSubset(CACHES_INITIAL, onSessionExpired);
  const { value, setValue, isDirty, discard, loading, error } = subset;
  if (error) return <Alert type="error" showIcon message="Failed to load" description={error} />;
  if (loading) return <PanelShell><Text type="secondary">Loading...</Text></PanelShell>;

  return (
    <PanelShell>
      <AdvancedApplyRail
        isDirty={isDirty}
        applying={subset.applying}
        onDiscard={discard}
        onApply={subset.runApply}
        applyOpen={subset.applyOpen}
        applyResponse={subset.applyResponse}
        cancelApply={subset.cancelApply}
        confirmApply={subset.confirmApply}
      />
      <div style={cardStyle}>
        <SectionHeader
          icon={<DatabaseOutlined />}
          title="Caches"
          description="In-memory caches the proxy uses to accelerate delta reconstruction and metadata lookups. Larger values trade memory for throughput."
        />
        <div style={{ marginTop: 16 }}>
          <FormField
            label={
              <>
                Reference cache size (MB)
                <RestartChip reason="cache_size_mb is set at engine construction. Moka caches cannot be resized live." />
              </>
            }
            yamlPath="advanced.cache_size_mb"
            helpText="LRU cache for delta-reconstruction reference files. Recommend ≥1024 MB for production."
            defaultPlaceholder="100"
            examples={[256, 1024, 4096]}
            onExampleClick={(v) => setValue({ ...value, cache_size_mb: Number(v) })}
          >
            <InputNumber
              value={value.cache_size_mb ?? undefined}
              onChange={(v) => setValue({ ...value, cache_size_mb: v ?? undefined })}
              min={16}
              style={{ width: 180, ...inputRadius }}
              addonAfter="MB"
            />
          </FormField>

          <FormField
            label="Metadata cache size (MB)"
            yamlPath="advanced.metadata_cache_mb"
            helpText="Object metadata cache for HEAD + LIST acceleration. Hot-reloadable."
            defaultPlaceholder="50"
            examples={[50, 200]}
            onExampleClick={(v) => setValue({ ...value, metadata_cache_mb: Number(v) })}
          >
            <InputNumber
              value={value.metadata_cache_mb ?? undefined}
              onChange={(v) => setValue({ ...value, metadata_cache_mb: v ?? undefined })}
              min={1}
              style={{ width: 180, ...inputRadius }}
              addonAfter="MB"
            />
          </FormField>

          <FormField
            label={
              <>
                Codec concurrency
                <RestartChip reason="xdelta3 subprocess permits are fixed at startup." />
              </>
            }
            yamlPath="advanced.codec_concurrency"
            helpText="Maximum concurrent xdelta3 subprocesses. Leave empty to auto-detect from CPU count."
            examples={[16, 40]}
            onExampleClick={(v) => setValue({ ...value, codec_concurrency: Number(v) })}
          >
            <InputNumber
              value={value.codec_concurrency ?? undefined}
              onChange={(v) => setValue({ ...value, codec_concurrency: v ?? undefined })}
              min={1}
              placeholder="auto"
              style={{ width: 180, ...inputRadius }}
            />
          </FormField>

          <FormField
            label={
              <>
                Tokio blocking threads
                <RestartChip reason="The tokio runtime's blocking pool is sized at process start." />
              </>
            }
            yamlPath="advanced.blocking_threads"
            helpText="Size of the tokio blocking pool. 512 by default. Smaller is often fine."
            examples={[64, 128, 512]}
            onExampleClick={(v) => setValue({ ...value, blocking_threads: Number(v) })}
          >
            <InputNumber
              value={value.blocking_threads ?? undefined}
              onChange={(v) => setValue({ ...value, blocking_threads: v ?? undefined })}
              min={1}
              placeholder="512"
              style={{ width: 180, ...inputRadius }}
            />
          </FormField>
        </div>
      </div>
    </PanelShell>
  );
}

// ───────────────────────────────────────────────────────────
// LimitsPanel  (env-var read-only)
// ───────────────────────────────────────────────────────────

export function LimitsPanel({ onSessionExpired }: PanelProps) {
  const { cardStyle, inputRadius } = useCardStyles();
  const { ACCENT_AMBER, TEXT_SECONDARY, BG_ELEVATED, BORDER, TEXT_MUTED } = useColors();
  const [config, setConfig] = useState<AdminConfig | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const cfg = await getAdminConfig();
        if (cancelled) return;
        if (!cfg) {
          onSessionExpired?.();
          return;
        }
        setConfig(cfg);
      } catch (e) {
        if (cancelled) return;
        setError(e instanceof Error ? e.message : 'Failed to load');
      }
    })();
    return () => { cancelled = true; };
  }, [onSessionExpired]);

  if (error) return <Alert type="error" showIcon message="Failed to load" description={error} />;
  if (!config) return <PanelShell><Text type="secondary">Loading...</Text></PanelShell>;

  const readOnlyField = (
    label: string,
    value: string | number,
    helpText: string,
    envName: string,
    envValue: string
  ) => (
    <div style={{ marginTop: 16 }}>
      <div style={{ fontSize: 11, fontWeight: 700, letterSpacing: 0.5, textTransform: 'uppercase', color: TEXT_MUTED, marginBottom: 4 }}>
        {label}
        <span
          title="Env-var only; no YAML field. Requires a process restart to change."
          style={{
            fontSize: 9,
            fontWeight: 700,
            letterSpacing: 0.5,
            padding: '1px 6px',
            borderRadius: 10,
            background: `${ACCENT_AMBER}20`,
            color: ACCENT_AMBER,
            marginLeft: 8,
          }}
        >
          Restart required
        </span>
      </div>
      <Input
        value={String(value)}
        readOnly
        style={{
          ...inputRadius,
          fontFamily: 'var(--font-mono)',
          fontSize: 13,
          opacity: 0.7,
        }}
      />
      <Text type="secondary" style={{ fontSize: 12, fontFamily: 'var(--font-ui)', display: 'block', marginTop: 4 }}>
        {helpText}
      </Text>
      <div
        style={{
          marginTop: 4,
          padding: '6px 10px',
          background: BG_ELEVATED,
          border: `1px solid ${BORDER}`,
          borderRadius: 6,
          fontSize: 11,
          fontFamily: 'var(--font-mono)',
          lineHeight: 1.6,
          color: TEXT_MUTED,
        }}
      >
        <span style={{ color: TEXT_SECONDARY }}>ENV:</span>&nbsp;{envName}={envValue}
        <br />
        <span style={{ fontStyle: 'italic', fontSize: 10 }}>
          environment-variable only — no YAML/config-file field.
        </span>
      </div>
    </div>
  );

  return (
    <PanelShell>
      <div style={cardStyle}>
        <SectionHeader
          icon={<CloudOutlined />}
          title="Request limits"
          description="Protect the server from overload and abuse. These are environment-variable driven — changing any of them requires a process restart."
        />
        {readOnlyField(
          'Request timeout (seconds)',
          config.request_timeout_secs,
          'Maximum time for any single request. Returns HTTP 504 Gateway Timeout when exceeded.',
          'DGP_REQUEST_TIMEOUT_SECS',
          String(config.request_timeout_secs)
        )}
        {readOnlyField(
          'Max concurrent requests',
          config.max_concurrent_requests,
          'Maximum in-flight HTTP requests. Additional requests queue until a slot opens.',
          'DGP_MAX_CONCURRENT_REQUESTS',
          String(config.max_concurrent_requests)
        )}
        {readOnlyField(
          'Max multipart uploads',
          config.max_multipart_uploads,
          'Maximum concurrent multipart uploads. Each holds part data in memory.',
          'DGP_MAX_MULTIPART_UPLOADS',
          String(config.max_multipart_uploads)
        )}
      </div>
    </PanelShell>
  );
}

// ───────────────────────────────────────────────────────────
// LoggingPanel  (hot-reloadable log_level)
// ───────────────────────────────────────────────────────────

const LOG_LEVEL_PRESETS = [
  { label: 'Error', value: 'deltaglider_proxy=error,tower_http=error' },
  { label: 'Warn', value: 'deltaglider_proxy=warn,tower_http=warn' },
  { label: 'Info', value: 'deltaglider_proxy=info,tower_http=info' },
  { label: 'Debug', value: 'deltaglider_proxy=debug,tower_http=debug' },
  { label: 'Trace', value: 'deltaglider_proxy=trace,tower_http=trace' },
] as const;

function normaliseFilter(filter: string): string {
  return filter
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
    .sort()
    .join(',');
}

function findMatchingPreset(logLevel: string): string | null {
  const canon = normaliseFilter(logLevel);
  for (const p of LOG_LEVEL_PRESETS) {
    if (normaliseFilter(p.value) === canon) return p.value;
  }
  return null;
}

export function LoggingPanel({ onSessionExpired }: PanelProps) {
  const { cardStyle, inputRadius } = useCardStyles();
  const subset = useAdvancedSubset(LOG_INITIAL, onSessionExpired);
  const { value, setValue, isDirty, discard, loading, error } = subset;
  const [custom, setCustom] = useState(false);

  // Sync custom flag from server-loaded value
  useEffect(() => {
    if (value.log_level && findMatchingPreset(value.log_level) == null) {
      setCustom(true);
    }
  }, [value.log_level]);

  if (error) return <Alert type="error" showIcon message="Failed to load" description={error} />;
  if (loading) return <PanelShell><Text type="secondary">Loading...</Text></PanelShell>;

  const currentPreset = value.log_level ? findMatchingPreset(value.log_level) : null;
  const radioValue = custom ? '__custom__' : currentPreset;

  return (
    <PanelShell>
      <AdvancedApplyRail
        isDirty={isDirty}
        applying={subset.applying}
        onDiscard={discard}
        onApply={subset.runApply}
        applyOpen={subset.applyOpen}
        applyResponse={subset.applyResponse}
        cancelApply={subset.cancelApply}
        confirmApply={subset.confirmApply}
      />
      <div style={cardStyle}>
        <SectionHeader
          icon={<ControlOutlined />}
          title="Log level"
          description="tracing-subscriber EnvFilter string. Hot-reloadable: changes take effect on the next request after Apply."
        />
        <div style={{ marginTop: 16 }}>
          <Radio.Group
            value={radioValue}
            onChange={(e) => {
              const v = e.target.value;
              if (v === '__custom__') {
                setCustom(true);
              } else {
                setCustom(false);
                setValue({ ...value, log_level: v });
              }
            }}
            style={{ display: 'flex', flexWrap: 'wrap', gap: 0 }}
          >
            {LOG_LEVEL_PRESETS.map((p) => (
              <Radio.Button key={p.value} value={p.value} style={{ fontSize: 13 }}>
                {p.label}
              </Radio.Button>
            ))}
            <Radio.Button value="__custom__" style={{ fontSize: 13 }}>
              Custom
            </Radio.Button>
          </Radio.Group>
          {custom && (
            <FormField
              label="Custom EnvFilter"
              yamlPath="advanced.log_level"
              helpText="Comma-separated tracing directives. See https://docs.rs/tracing-subscriber for syntax."
              examples={[
                'deltaglider_proxy=debug,tower_http=info',
                'info,deltaglider_proxy::api=trace',
              ]}
              onExampleClick={(v) => setValue({ ...value, log_level: String(v) })}
              style={{ marginTop: 12 }}
            >
              <Input
                value={value.log_level ?? ''}
                onChange={(e) => setValue({ ...value, log_level: e.target.value || undefined })}
                placeholder="deltaglider_proxy=debug,tower_http=info"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
              />
            </FormField>
          )}
        </div>
      </div>
    </PanelShell>
  );
}

// ───────────────────────────────────────────────────────────
// ConfigDbSyncPanel  (advanced.config_sync_bucket)
// ───────────────────────────────────────────────────────────

export function ConfigDbSyncPanel({ onSessionExpired }: PanelProps) {
  const { cardStyle, inputRadius } = useCardStyles();
  const subset = useAdvancedSubset(SYNC_INITIAL, onSessionExpired);
  const { value, setValue, isDirty, discard, loading, error } = subset;
  if (error) return <Alert type="error" showIcon message="Failed to load" description={error} />;
  if (loading) return <PanelShell><Text type="secondary">Loading...</Text></PanelShell>;

  const enabled = !!(value.config_sync_bucket && value.config_sync_bucket.length > 0);

  return (
    <PanelShell>
      <AdvancedApplyRail
        isDirty={isDirty}
        applying={subset.applying}
        onDiscard={discard}
        onApply={subset.runApply}
        applyOpen={subset.applyOpen}
        applyResponse={subset.applyResponse}
        cancelApply={subset.cancelApply}
        confirmApply={subset.confirmApply}
      />
      <div style={cardStyle}>
        {/* Header + status pill on the same baseline. The pill
            (Disabled / Active / Pending restart) replaces the old
            "Multi-instance setups only" info banner — that banner
            restated the panel description without adding information.
            Who-this-is-for is covered by the lead sentence; state
            lives next to the title where the operator's eye lands
            first. */}
        <div
          style={{
            display: 'flex',
            alignItems: 'flex-start',
            justifyContent: 'space-between',
            gap: 16,
            marginBottom: 16,
          }}
        >
          <SectionHeader
            icon={<SyncOutlined />}
            title="Config DB sync"
            description="For multi-instance deployments. Replicates the encrypted IAM database through an S3 bucket so every proxy instance shares the same users, groups, OAuth providers, and mapping rules."
          />
          <SyncStatusPill
            enabled={enabled}
            dirty={isDirty}
            bucket={value.config_sync_bucket}
          />
        </div>

        <FormField
          label={
            <>
              Sync bucket
              <RestartChip reason="The ConfigDbSync background task starts at process boot and is not rebuilt on hot-reload." />
            </>
          }
          yamlPath="advanced.config_sync_bucket"
          helpText="S3 bucket name on the default backend. All instances must point at the same bucket. Sync uses periodic full replacement — the newest ETag wins."
          examples={['dgp-iam-state', 'prod-dgp-config']}
          onExampleClick={(v) =>
            setValue({ ...value, config_sync_bucket: String(v) })
          }
        >
          <Input
            value={value.config_sync_bucket ?? ''}
            onChange={(e) =>
              setValue({
                ...value,
                config_sync_bucket: e.target.value || undefined,
              })
            }
            placeholder="Leave empty to disable"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
        </FormField>
      </div>
    </PanelShell>
  );
}

/**
 * Inline pill showing what `config_sync_bucket` currently resolves
 * to from the operator's perspective. Three states in this panel:
 *
 *   - Disabled: the field is empty, sync is off.
 *   - Pending restart: the field has a bucket, but the operator has
 *     unsaved edits — the running background task hasn't picked up
 *     the new bucket yet.
 *   - Active: the field matches the running config.
 *
 * The "active after restart vs active now" distinction mirrors the
 * RestartChip semantics on the field — hot-reload doesn't rebuild
 * this background task. The pill uses the same colour language as
 * the rest of the admin UI: teal = good/default, amber = needs
 * attention, neutral muted = off.
 */
function SyncStatusPill({
  enabled,
  dirty,
  bucket,
}: {
  enabled: boolean;
  dirty: boolean;
  bucket?: string;
}) {
  const { ACCENT_BLUE, ACCENT_AMBER, TEXT_MUTED, BORDER } = useColors();

  let dotColor: string;
  let bg: string;
  let border: string;
  let label: string;

  if (dirty && enabled) {
    dotColor = ACCENT_AMBER;
    bg = 'rgba(251, 191, 36, 0.08)';
    border = 'rgba(251, 191, 36, 0.28)';
    label = 'Pending restart';
  } else if (enabled) {
    dotColor = ACCENT_BLUE;
    bg = 'rgba(45, 212, 191, 0.08)';
    border = 'rgba(45, 212, 191, 0.28)';
    label = `Active · ${bucket}`;
  } else {
    dotColor = TEXT_MUTED;
    bg = 'transparent';
    border = BORDER;
    label = 'Disabled';
  }

  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 8,
        padding: '6px 12px',
        background: bg,
        border: `1px solid ${border}`,
        borderRadius: 999,
        fontSize: 12,
        fontFamily: 'var(--font-ui)',
        whiteSpace: 'nowrap',
        flexShrink: 0,
        marginTop: 4,
      }}
    >
      <span
        aria-hidden
        style={{
          width: 8,
          height: 8,
          borderRadius: '50%',
          background: dotColor,
          boxShadow: enabled && !dirty ? `0 0 6px ${dotColor}80` : 'none',
        }}
      />
      {label}
    </span>
  );
}
