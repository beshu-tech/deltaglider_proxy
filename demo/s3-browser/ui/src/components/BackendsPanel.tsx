import { useEffect, useMemo, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { Button, Input, Modal, Radio, Switch, Typography, Space, Alert, Spin } from 'antd';
import { PlusOutlined, DeleteOutlined, DatabaseOutlined, CloudOutlined, CheckCircleOutlined, ApiOutlined } from '@ant-design/icons';
import type { BackendInfo, CreateBackendRequest } from '../adminApi';
import { createBackend, deleteBackend, testS3Connection, updateAdminConfig, putSection } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { useBackends, useBucketOrigins } from '../queries/backends';
import CreateBucketModal from './CreateBucketModal';
import ReencryptProposalModal, { type ReencryptTransition } from './ReencryptProposalModal';
import { useColors } from '../ThemeContext';
import { useCardStyles, contentColumn, CONTENT_FORM } from './shared-styles';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import BackendEncryptionEditor, { type BackendEncryptionPatch } from './BackendEncryptionEditor';
import { buildEncryptionSectionBody } from '../backendEncryptionPayload';
import MaskedSecretInput from './MaskedSecretInput';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function BackendsPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();
  // Query client lets mutations close the loop with `invalidateQueries`
  // instead of the local `refresh()` having to coordinate with siblings.
  const qc = useQueryClient();

  // Backends + config are read from the shared cache; mutations below invalidate
  // `qk.backends.list()` + `qk.config()` (via refresh()) so all readers refresh.
  const backendsQuery = useBackends();
  const backends = backendsQuery.data?.backends ?? [];
  const defaultBackend = backendsQuery.data?.default_backend ?? null;
  const { data: config } = useAdminConfig();

  // Bucket counts per backend, from the authoritative virtual→backend origin
  // map (works for filesystem AND s3; testS3Connection is s3-only + transient).
  // Buckets with no explicit backend_name route to the default backend.
  const originsQuery = useBucketOrigins();
  const countByBackend = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const o of originsQuery.data?.buckets ?? []) {
      const key = o.backend_name ?? defaultBackend ?? '';
      if (!key) continue;
      counts[key] = (counts[key] ?? 0) + 1;
    }
    return counts;
  }, [originsQuery.data, defaultBackend]);

  // "Create bucket here" → opens the shared CreateBucketModal pre-targeted to a
  // specific backend. This panel only renders for admins (admin route).
  const [createBucketBackend, setCreateBucketBackend] = useState<string | null>(null);
  // Post-encryption-apply proposal: offer the one-off re-encrypt job for
  // the changed backend's buckets (enable / rotate / disable).
  const [reencryptProposal, setReencryptProposal] = useState<{
    transition: ReencryptTransition;
    backendName: string;
    buckets: string[];
  } | null>(null);
  const loading = backendsQuery.isLoading;
  // Surface a load error (401 bubbles to the session-expired handler; anything
  // else renders in the Alert below).
  const loadError = backendsQuery.error;
  // Effect, not render-body: react-query keeps `error` populated across renders.
  useEffect(() => {
    if (loadError instanceof Error && loadError.message.includes('401')) {
      onSessionExpired?.();
    }
  }, [loadError, onSessionExpired]);
  const error =
    loadError && !(loadError instanceof Error && loadError.message.includes('401'))
      ? loadError instanceof Error
        ? loadError.message
        : 'Failed to load'
      : null;

  // New backend form
  const [showForm, setShowForm] = useState(false);
  const [formName, setFormName] = useState('');
  const [formType, setFormType] = useState<'filesystem' | 's3'>('filesystem');
  const [formPath, setFormPath] = useState('./data');
  const [formEndpoint, setFormEndpoint] = useState('');
  const [formRegion, setFormRegion] = useState('us-east-1');
  const [formForcePathStyle, setFormForcePathStyle] = useState(true);
  const [formAccessKey, setFormAccessKey] = useState('');
  const [formSecretKey, setFormSecretKey] = useState('');
  const [formSetDefault, setFormSetDefault] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<{ ok: boolean; message: string } | null>(null);

  const [testingBackend, setTestingBackend] = useState<string | null>(null);
  const [testResult, setTestResult] = useState<{ name: string; ok: boolean; message: string } | null>(null);

  // After a mutation, invalidate the shared cache so any panel reading
  // backends/config (this panel's own `useBackends()` + `useAdminConfig()`,
  // CredentialsModePanel, BucketsPanel, …) refetches the freshly-saved state.
  // The list READ itself is now `useBackends()` — no manual setState.
  const refresh = async () => {
    await Promise.all([
      qc.invalidateQueries({ queryKey: qk.backends.list() }),
      qc.invalidateQueries({ queryKey: qk.config() }),
    ]);
  };

  const handleCreate = async () => {
    setSaving(true);
    setSaveResult(null);
    const req: CreateBackendRequest = {
      name: formName.trim(),
      type: formType,
      set_default: formSetDefault || backends.length === 0,
    };
    if (formType === 'filesystem') {
      req.path = formPath;
    } else {
      req.endpoint = formEndpoint || undefined;
      req.region = formRegion;
      req.force_path_style = formForcePathStyle;
      if (formAccessKey) req.access_key_id = formAccessKey;
      if (formSecretKey) req.secret_access_key = formSecretKey;
    }
    try {
      // S3 backends: prove the credentials work BEFORE saving anything —
      // creating a backend blind and discovering a typo'd secret later (via
      // failing uploads) is the worse failure mode.
      if (formType === 's3') {
        if (!formAccessKey || !formSecretKey) {
          setSaveResult({
            ok: false,
            message: 'S3 backends need both an Access Key ID and a Secret Access Key.',
          });
          return;
        }
        const probe = await testS3Connection({
          endpoint: req.endpoint,
          region: req.region,
          force_path_style: req.force_path_style,
          access_key_id: req.access_key_id,
          secret_access_key: req.secret_access_key,
        });
        if (!probe.success) {
          setSaveResult({
            ok: false,
            message: `Connection test failed — backend not created: ${probe.error || 'unknown error'}`,
          });
          return;
        }
      }
      const result = await createBackend(req);
      if (result.success) {
        setSaveResult({
          ok: true,
          message: `Backend '${formName.trim()}' created${formType === 's3' ? ' (connection verified)' : ''} — use "Create bucket here" on its card to start using it`,
        });
        setShowForm(false);
        resetForm();
        await refresh();
      } else {
        setSaveResult({ ok: false, message: result.error || 'Failed to create backend' });
      }
    } catch (e) {
      setSaveResult({ ok: false, message: e instanceof Error ? e.message : 'Network error' });
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = (name: string) => {
    const routedHere = countByBackend[name] ?? 0;
    Modal.confirm({
      title: `Remove backend "${name}"?`,
      okText: 'Remove backend',
      okButtonProps: { danger: true },
      content: (
        <Text type="secondary" style={{ fontSize: 13 }}>
          {routedHere > 0 ? (
            <>
              <strong>{routedHere} bucket{routedHere === 1 ? '' : 's'}</strong> currently route
              {routedHere === 1 ? 's' : ''} here — their objects stay on disk but become
              unreachable until re-routed or migrated.{' '}
            </>
          ) : (
            'No buckets route here. '
          )}
          The stored data itself is not deleted.
        </Text>
      ),
      onOk: async () => {
        try {
          const result = await deleteBackend(name);
          if (result.success) {
            setSaveResult({ ok: true, message: `Backend '${name}' removed` });
            await refresh();
          } else {
            setSaveResult({ ok: false, message: result.error || 'Failed to delete' });
          }
        } catch (e) {
          setSaveResult({ ok: false, message: e instanceof Error ? e.message : 'Network error' });
        }
      },
    });
  };

  const handleTestConnection = async (b: BackendInfo) => {
    if (b.backend_type !== 's3') return;
    setTestingBackend(b.name);
    setTestResult(null);
    try {
      const result = await testS3Connection({
        endpoint: b.endpoint || undefined,
        region: b.region || undefined,
        force_path_style: b.force_path_style ?? true,
      });
      setTestResult({
        name: b.name,
        ok: result.success,
        message: result.success
          ? `Connected — ${result.buckets?.length ?? 0} bucket(s)`
          : result.error || 'Connection failed',
      });
    } catch {
      setTestResult({ name: b.name, ok: false, message: 'Network error' });
    } finally {
      setTestingBackend(null);
    }
  };

  const resetForm = () => {
    setFormName(''); setFormType('filesystem'); setFormPath('./data');
    setFormEndpoint(''); setFormRegion('us-east-1'); setFormForcePathStyle(true);
    setFormAccessKey(''); setFormSecretKey(''); setFormSetDefault(false);
  };

  /**
   * Apply a per-backend encryption change via a targeted `storage`
   * section PUT. The wire body (singleton vs named-list shape) is
   * composed by the pure `buildEncryptionSectionBody` builder; this
   * handler owns only the I/O + result-message bookkeeping around it.
   */
  const handleEncryptionApply = async (
    backendName: string,
    patch: BackendEncryptionPatch,
  ): Promise<void> => {
    // Compose the section-PUT body via the pure builder (singleton vs
    // named-list shape, encryption-block translation). Extracted to
    // `backendEncryptionPayload.ts` so the wire shape is unit-tested
    // byte-for-byte and can't silently drift.
    const body = buildEncryptionSectionBody(backendName, patch, backends);

    // Tracks whether the try-block already set a precise result message this
    // run. Using a local (not the closed-over `saveResult` state, which is the
    // stale render snapshot) is what keeps the catch from either suppressing a
    // genuine error — because a PREVIOUS success message is still in state — or
    // clobbering the precise message just set with a generic one.
    let resultSet = false;
    try {
      const result = await putSection('storage', body);
      if (!result.ok) {
        setSaveResult({
          ok: false,
          message: result.error || 'Failed to apply encryption change',
        });
        resultSet = true;
        throw new Error(result.error || 'Apply failed');
      }
      setSaveResult({
        ok: true,
        message: `Encryption updated on backend '${backendName}'`,
      });
      await refresh();
      // Existing objects keep their OLD at-rest state — propose the canned
      // migration when the transition changes what "correct" looks like.
      // prevMode comes from the pre-apply backends snapshot (render closure).
      const prevMode = backends.find((b) => b.name === backendName)?.encryption?.mode;
      const transition: ReencryptTransition | null =
        patch.mode === 'aes256-gcm-proxy'
          ? prevMode === 'aes256-gcm-proxy'
            ? patch.key
              ? 'rotate'
              : null // settings-only change (e.g. legacy shim) — nothing to rewrite
            : 'encrypt'
          : patch.mode === 'none' && prevMode === 'aes256-gcm-proxy'
            ? 'decrypt'
            : null;
      if (transition) {
        const bucketsHere = (originsQuery.data?.buckets ?? [])
          .filter((o) => (o.backend_name ?? defaultBackend) === backendName)
          .map((o) => o.name)
          .sort();
        setReencryptProposal({ transition, backendName, buckets: bucketsHere });
      }
    } catch (e) {
      if (e instanceof Error && !resultSet) {
        setSaveResult({ ok: false, message: e.message });
      }
      throw e;
    }
  };

  const globalCompressionOn = (config?.max_delta_ratio ?? 0.75) > 0;

  if (loading) {
    return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin /></div>;
  }

  return (
    <div style={contentColumn(CONTENT_FORM)}>
      <ReencryptProposalModal
        open={reencryptProposal !== null}
        transition={reencryptProposal?.transition ?? 'encrypt'}
        backendName={reencryptProposal?.backendName ?? ''}
        buckets={reencryptProposal?.buckets ?? []}
        onClose={() => setReencryptProposal(null)}
      />
      <CreateBucketModal
        open={createBucketBackend !== null}
        presetBackend={createBucketBackend ?? undefined}
        canAdmin
        onClose={() => setCreateBucketBackend(null)}
        onCreated={() => {
          // Refresh the per-backend counts (and the backend list, in case the
          // default-backend assignment shifted).
          qc.invalidateQueries({ queryKey: qk.backends.origins() });
          qc.invalidateQueries({ queryKey: qk.backends.list() });
        }}
      />
      <Space direction="vertical" size={0} style={{ width: '100%' }}>

        {saveResult && (
          <Alert type={saveResult.ok ? 'success' : 'error'} message={saveResult.message} showIcon closable onClose={() => setSaveResult(null)} style={{ borderRadius: 8, marginBottom: 12 }} />
        )}
        {error && (
          <Alert type="error" message={error} showIcon style={{ borderRadius: 8, marginBottom: 12 }} />
        )}

        {/* Storage Backends — the page's subject, first. */}
        <div style={cardStyle}>
          <SectionHeader
            icon={<DatabaseOutlined />}
            title="Storage Backends"
            description={
              backends.length === 0
                ? 'No backend configured.'
                : backends.every((b) => b.is_synthesized)
                  ? 'Running on a single unnamed backend (below). Add a named backend to move to the multi-backend setup.'
                  : `${backends.length} backend${backends.length !== 1 ? 's' : ''} configured.`
            }
          />

          {backends.map((b) => (
            <div key={b.name} style={{
              marginTop: 12, padding: '12px 14px',
              border: `1px solid ${b.name === defaultBackend ? colors.ACCENT_BLUE + '66' : colors.BORDER}`,
              borderRadius: 8,
              background: b.name === defaultBackend ? colors.ACCENT_BLUE + '08' : colors.BG_ELEVATED,
            }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', rowGap: 8 }}>
                {b.backend_type === 'filesystem'
                  ? <DatabaseOutlined style={{ fontSize: 16, color: colors.ACCENT_BLUE }} />
                  : <CloudOutlined style={{ fontSize: 16, color: colors.ACCENT_BLUE }} />}
                <div style={{ flex: '1 1 180px', minWidth: 0 }}>
                  <Text strong style={{ fontFamily: 'var(--font-ui)', fontSize: 14 }}>{b.name}</Text>
                  {b.name === defaultBackend && (
                    <span style={{ fontSize: 10, color: colors.ACCENT_BLUE, marginLeft: 8, fontWeight: 600 }}>DEFAULT</span>
                  )}
                  {b.is_synthesized && (
                    <span
                      style={{ fontSize: 10, color: colors.ACCENT_AMBER, marginLeft: 8, fontWeight: 600 }}
                      title="Virtual projection of the legacy singleton `storage.backend` in YAML. Not a real named backend; cannot be deleted. Add a named backend to migrate."
                    >
                      LEGACY SINGLETON
                    </span>
                  )}
                  <div style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: 'var(--font-mono)', overflowWrap: 'anywhere' }}>
                    {b.backend_type === 'filesystem'
                      ? `filesystem: ${b.path}`
                      : `s3: ${b.endpoint || 'AWS'} (${b.region})`}
                  </div>
                  <div style={{ fontSize: 11, color: colors.TEXT_MUTED, marginTop: 2 }}>
                    {(() => {
                      const n = countByBackend[b.name] ?? 0;
                      return n === 0
                        ? "No buckets routed here yet — use 'Create bucket here' to add one."
                        : `${n} bucket${n === 1 ? '' : 's'} routed here`;
                    })()}
                  </div>
                </div>
                <Button
                  size="small"
                  icon={<PlusOutlined />}
                  onClick={() => setCreateBucketBackend(b.name)}
                  title="Create a bucket on this backend"
                >
                  Create bucket here
                </Button>
                {b.backend_type === 's3' && (
                  <Button size="small" icon={<ApiOutlined />} loading={testingBackend === b.name} onClick={() => handleTestConnection(b)} title="Test the S3 connection">
                    Test
                  </Button>
                )}
                {!b.is_synthesized && (
                  <Button size="small" icon={<DeleteOutlined />} danger onClick={() => handleDelete(b.name)} title="Remove backend" />
                )}
              </div>
              {testResult?.name === b.name && (
                <Alert type={testResult.ok ? 'success' : 'error'} message={testResult.message} showIcon style={{ marginTop: 8, borderRadius: 6 }} />
              )}
              {/* Per-backend encryption subsection: shows the current
                 mode, exposes a mode-change picker, and wraps the
                 proxy-AES key-generation flow. Apply sends a targeted
                 storage section PUT; siblings preserved by merge-patch. */}
              <BackendEncryptionEditor
                backendName={b.name}
                current={b.encryption}
                onApply={(patch) => handleEncryptionApply(b.name, patch)}
              />
            </div>
          ))}

          {!showForm && (
            <Button data-testid="backends-add" aria-label="Add backend" icon={<PlusOutlined />} onClick={() => setShowForm(true)} style={{ marginTop: 12, borderRadius: 8, fontFamily: 'var(--font-ui)', fontWeight: 600 }} block type="dashed">
              Add Backend
            </Button>
          )}
        </div>

        {/* New Backend Form */}
        {showForm && (
          <div style={cardStyle}>
            <SectionHeader icon={<PlusOutlined />} title="New Backend" />
            <div>
              <FormField label="Name" yamlPath="storage.backends[].name">
                <Input data-testid="backend-name" aria-label="Backend name" value={formName} onChange={(e) => setFormName(e.target.value)} placeholder="e.g. local, hetzner, aws-prod" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
              </FormField>
              <FormField label="Type" yamlPath="storage.backends[].type">
                <Radio.Group value={formType} onChange={(e) => setFormType(e.target.value)} style={{ display: 'flex', gap: 0 }}>
                  <Radio.Button value="filesystem" style={{ fontSize: 13 }}>Filesystem</Radio.Button>
                  <Radio.Button value="s3" style={{ fontSize: 13 }}>S3</Radio.Button>
                </Radio.Group>
              </FormField>
              {formType === 'filesystem' && (
                <FormField label="Data Directory" yamlPath="storage.backends[].path">
                  <Input value={formPath} onChange={(e) => setFormPath(e.target.value)} placeholder="./data" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                </FormField>
              )}
              {formType === 's3' && (
                <>
                  <FormField label="Endpoint" yamlPath="storage.backends[].endpoint">
                    <Input value={formEndpoint} onChange={(e) => setFormEndpoint(e.target.value)} placeholder="https://fsn1.your-objectstorage.com" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <FormField label="Region" yamlPath="storage.backends[].region">
                    <Input value={formRegion} onChange={(e) => setFormRegion(e.target.value)} placeholder="us-east-1" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <div style={{ marginBottom: 20 }}>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                      <Switch checked={formForcePathStyle} onChange={setFormForcePathStyle} size="small" />
                      <Text style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Force path-style URLs</Text>
                    </div>
                    <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 4 }}>
                      Keep ON for S3-compatibles (MinIO, Hetzner, Ceph). Turn OFF for AWS S3.
                    </Text>
                  </div>
                  <FormField label="Access Key ID" yamlPath="storage.backends[].access_key_id">
                    <Input value={formAccessKey} onChange={(e) => setFormAccessKey(e.target.value)} placeholder="AKIAIOSFODNN7EXAMPLE" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <FormField label="Secret Access Key" yamlPath="storage.backends[].secret_access_key">
                    <MaskedSecretInput
                      mode="blank-keeps"
                      value={formSecretKey}
                      onChange={setFormSecretKey}
                      placeholder="wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLE"
                      style={{ ...inputRadius }}
                    />
                  </FormField>
                </>
              )}
            </div>
            <div style={{ marginTop: 12, display: 'flex', alignItems: 'center', gap: 8 }}>
              <Switch checked={formSetDefault} onChange={setFormSetDefault} size="small" />
              <Text style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Set as default backend</Text>
            </div>
            <div style={{ marginTop: 16, display: 'flex', gap: 8 }}>
              <Button data-testid="backend-create" aria-label="Create backend" type="primary" icon={<CheckCircleOutlined />} onClick={handleCreate} loading={saving} disabled={!formName.trim()} style={{ flex: 1, borderRadius: 8, fontWeight: 600 }}>
                Create Backend
              </Button>
              <Button onClick={() => { setShowForm(false); resetForm(); }} style={{ borderRadius: 8 }}>Cancel</Button>
            </div>
          </div>
        )}

        {/* Defaults — global knobs new buckets inherit. Demoted below the
            backends themselves (the page's subject); per-bucket overrides
            live on the Buckets page. */}
        <div style={cardStyle}>
          <SectionHeader
            icon={<CheckCircleOutlined />}
            title="Defaults"
            description="What new buckets inherit. Per-bucket overrides on the Buckets page always win."
          />
          <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginTop: 12 }}>
            <Switch
              data-testid="delta-compression-switch"
              aria-label="Delta compression default"
              checked={globalCompressionOn}
              onChange={async (on) => {
                try {
                  await updateAdminConfig({ max_delta_ratio: on ? 0.75 : 0 });
                  await refresh();
                } catch { /* non-blocking: user sees the toggle revert */ }
              }}
            />
            <div>
              <Text style={{ fontSize: 13, fontWeight: 700, fontFamily: 'var(--font-ui)', color: colors.TEXT_PRIMARY }}>
                Delta compression: <span style={{ color: globalCompressionOn ? colors.ACCENT_GREEN : colors.ACCENT_AMBER }}>{globalCompressionOn ? 'ON' : 'OFF'}</span>
                <span style={{ fontWeight: 400, color: colors.TEXT_MUTED, marginLeft: 8, fontSize: 12 }}>applies immediately</span>
              </Text>
              <Text type="secondary" style={{ fontSize: 12, fontFamily: 'var(--font-ui)', display: 'block', marginTop: 2, lineHeight: 1.6 }}>
                {globalCompressionOn
                  ? 'Versioned binaries are stored as xdelta3 deltas (30-70% smaller); reads reconstruct transparently. Already-compressed formats (images, video) are skipped.'
                  : 'Files are stored as-is. Compression can still be enabled per bucket.'}
              </Text>
            </div>
          </div>
        </div>

      </Space>
    </div>
  );
}
