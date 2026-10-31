import { useEffect, useMemo, useRef, useState } from 'react';
import { LinkifiedText } from './LinkifiedText';
import { CAPABILITY_DOC_URL } from '../linkifyDocUrl';
import { useQueryClient } from '@tanstack/react-query';
import { qk } from '../queries/keys';
import { Button, Input, Radio, Switch, Typography, Space, Alert, message } from 'antd';
import { ErrorState, LoadingState } from './StatePlaceholders';
import { PlusOutlined, DeleteOutlined, DatabaseOutlined, CheckCircleOutlined, ApiOutlined } from '@ant-design/icons';
import type { BackendEncryptionMode, BackendHealthEntry, BackendInfo, ConfirmHeaders, ConversionView, CreateBackendRequest } from '../adminApi';
import { conversionCountText, offersPurge, versionedWarnings } from '../conversionView';
import PurgeVersionsBox from './PurgeVersionsBox';
import { confirmDialog } from '../confirmDialog';
import { backendHealthSentence, createBackend, deleteBackend, plainError, probeBackend, testS3Connection, updateAdminConfig, getSectionVersioned, validateSection } from '../adminApi';
import type { PlainError } from '../adminApi';
import PlainErrorAlert from './PlainErrorAlert';
import { applySection, persistFailureText, sectionApplyErrorText } from '../applySection';
import { useAdminConfig } from '../queries/config';
import { useBackends, useBucketOrigins } from '../queries/backends';
import CreateBucketModal from './CreateBucketModal';
import { backendFacts, backendKind } from '../backendSummary';
import BackendCard, { type BackendPill } from './BackendCard';
import ReencryptProposalModal from './ReencryptProposalModal';
import {
  backendBucketCountText,
  backendBucketCounts,
  bucketsWhoseEncryptionChanged,
  dominantTransition,
  type ReencryptTransition,
} from '../bucketEncryption';
import { useColors } from '../ThemeContext';
import { useCardStyles, contentColumn, CONTENT_FORM, SPACE } from './shared-styles';
import SectionHeader from './SectionHeader';
import PageActions from './PageActions';
import FormField from './FormField';
import BackendEncryptionEditor, { type BackendEncryptionPatch } from './BackendEncryptionEditor';
import {
  buildClearLegacySectionBody,
  buildEncryptionSectionBody,
  type StorageSectionBackends,
} from '../backendEncryptionPayload';
import MaskedSecretInput from './MaskedSecretInput';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { isSessionExpired } from '../errorHandling';
import { ABSOLUTE_PATH_ERROR, isAbsolutePath } from '../utils';
import RowActionsMenu, { type RowActionConfirm } from './RowActionsMenu';
import { backendEncryptionEnvPath, envOverrideHelp, envOverrideSource, findEnvOverride } from '../envOverrides';

const { Text } = Typography;

/**
 * A storage write that may start conversions (turning on proxy AES):
 * validate it, and when it starts any over existing
 * objects, name how many and ask first, with the purge box for a
 * versioned bucket. Returns the confirmation headers to apply with, or
 * `null` when the operator cancelled.
 */
async function reviewConversions(
  title: string,
  okText: string,
  body: unknown,
  confirm: ConfirmHeaders | undefined,
): Promise<ConfirmHeaders | undefined | null> {
  const plan = await validateSection('storage', body, confirm);
  const text = conversionCountText(plan.conversion_preview);
  if (!plan.ok || !text) return confirm;
  // The dialog's content is static: the box reports into a local.
  let purge = false;
  const ok = await confirmDialog({
    title,
    content: (
      <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xs }}>
        <span data-testid="conversion-count">{text}</span>
        {versionedWarnings(plan.conversion_preview).map((w) => (
          <Text key={w} type="secondary" data-testid="conversion-versioned">
            {w}
          </Text>
        ))}
        {offersPurge(plan.conversion_preview) && (
          <PurgeVersionsBox
            onChange={(v) => {
              purge = v;
            }}
          />
        )}
      </div>
    ),
    okText,
  });
  if (!ok) return null;
  return purge ? { ...confirm, purgePlaintextVersions: true } : confirm;
}

/** Plain-language one-liner for a health verdict (badge + Test result). */
function healthLabel(h: BackendHealthEntry): string {
  return h.status === 'healthy' ? 'Connected' : backendHealthSentence(h.status, h.detail);
}

/** The status pills after a backend's name: default and health. The
 *  unnamed YAML backend has no pill ("Legacy singleton" was an internal
 *  word): the section description says what it is. */
function backendPills(b: BackendInfo, isDefault: boolean, colors: ReturnType<typeof useColors>): BackendPill[] {
  const pills: BackendPill[] = [];
  if (isDefault) {
    pills.push({ label: 'Default', color: colors.ACCENT_BLUE, title: 'Buckets without a backend of their own are created here.' });
  }
  const h = b.health;
  if (h) {
    const ok = h.status === 'healthy';
    pills.push({
      label: ok ? 'Connected' : h.status === 'auth-rejected' ? 'Credentials rejected' : h.status === 'unreachable' ? 'Unreachable' : 'Erroring',
      color: ok ? colors.ACCENT_GREEN : h.status === 'auth-rejected' ? colors.ACCENT_RED : colors.ACCENT_AMBER,
      dot: true,
      title: `${healthLabel(h)} · probed ${new Date(h.probed_at * 1000).toLocaleString()}`,
    });
  }
  return pills;
}

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
  const { data: config } = useAdminConfig({ onSessionExpired });

  // Bucket counts per backend, from the authoritative virtual→backend origin
  // map (works for filesystem AND s3; testS3Connection is s3-only + transient).
  // The server resolves `backend_name` (routes, aliases, the default).
  const originsQuery = useBucketOrigins();
  const countByBackend = useMemo(() => {
    const counts: Record<string, number> = {};
    for (const o of originsQuery.data?.buckets ?? []) {
      if (!o.backend_name) continue;
      counts[o.backend_name] = (counts[o.backend_name] ?? 0) + 1;
    }
    return counts;
  }, [originsQuery.data]);
  // Per backend: buckets that take its encryption and buckets with their own.
  const encryptionCounts = useMemo(
    () => backendBucketCounts(originsQuery.data?.buckets ?? []),
    [originsQuery.data]
  );
  // The open conversions of the buckets that follow each backend's
  // encryption (the backend card shows one status row each).
  const backendConversions = useMemo(() => {
    const out: Record<string, Array<{ bucket: string; view: ConversionView }>> = {};
    for (const o of originsQuery.data?.buckets ?? []) {
      const e = o.effective_encryption;
      if (e && e.source !== 'bucket' && e.conversion?.run) {
        (out[e.backend] ??= []).push({ bucket: o.name, view: e.conversion });
      }
    }
    return out;
  }, [originsQuery.data]);

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
  useSessionExpiredOn(loadError, onSessionExpired);
  // A failed first load is the page's one state (ErrorState with Retry);
  // a failed background refetch keeps the rows it has.
  const listError = loadError && !isSessionExpired(loadError) && !backendsQuery.data ? loadError : null;

  // New backend form
  const [showForm, setShowForm] = useState(false);
  // The "New backend" button is in the page header, far above the form:
  // bring the form into view when it opens.
  const formRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (showForm) formRef.current?.scrollIntoView?.({ block: 'nearest', behavior: 'smooth' });
  }, [showForm]);
  const [formName, setFormName] = useState('');
  const [formType, setFormType] = useState<'filesystem' | 's3'>('filesystem');
  // No prefill: a relative path such as ./data resolves against the proxy's
  // working directory, which differs between systemd, Docker and a shell.
  const [formPath, setFormPath] = useState('');
  const [formEndpoint, setFormEndpoint] = useState('');
  const [formRegion, setFormRegion] = useState('us-east-1');
  const [formForcePathStyle, setFormForcePathStyle] = useState(true);
  const [formAccessKey, setFormAccessKey] = useState('');
  const [formSecretKey, setFormSecretKey] = useState('');
  const [formSetDefault, setFormSetDefault] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveResult, setSaveResult] = useState<{ ok: boolean; message: string } | null>(null);
  // Create errors render inside the form, next to the button that caused them;
  // the page-top Alert is out of view when the form is open.
  const [formError, setFormError] = useState<PlainError | null>(null);
  const pathInvalid = formType === 'filesystem' && !isAbsolutePath(formPath);
  // Set by the first Create click: from then on, empty required fields say
  // so on the field itself (a disabled Create button said nothing).
  const [createTried, setCreateTried] = useState(false);
  const nameMissing = !formName.trim();

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
    setCreateTried(true);
    if (nameMissing || pathInvalid) return;
    setSaving(true);
    setSaveResult(null);
    setFormError(null);
    const req: CreateBackendRequest = {
      name: formName.trim(),
      type: formType,
      set_default: formSetDefault || backends.length === 0,
    };
    if (formType === 'filesystem') {
      req.path = formPath.trim();
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
          setFormError({ text: 'An S3 backend needs an access key ID and a secret access key.' });
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
          const why = plainError(probe.error || '', 'The connection test failed.');
          setFormError({ text: `${why.text} The backend was not created.`, details: why.details });
          return;
        }
      }
      // A new backend starts plaintext. As the new default it makes every
      // bucket without a route store plaintext where the current default
      // encrypts: confirm that first, then send `route:default`.
      const defaultEncrypts = backends.some((b) => b.name === defaultBackend && b.encryption.mode !== 'none');
      let confirm: ConfirmHeaders | undefined;
      if (req.set_default && defaultEncrypts) {
        const ok = await confirmDialog({
          title: `Make ${req.name} the default backend?`,
          content: `The default backend ${defaultBackend} encrypts. ${req.name} starts without encryption, so new writes to every bucket without its own route or encryption are stored as plaintext.`,
          okText: 'Make it the default',
          danger: true,
        });
        if (!ok) return;
        confirm = { routeDefault: true };
      }
      const result = await createBackend(req, confirm);
      if (result.success) {
        const name = formName.trim();
        // A toast is visible wherever the page is scrolled; then bring the
        // new card into view so the operator sees what was created.
        message.success(
          `Backend "${name}" created${formType === 's3' ? ' (connection verified)' : ''}. Use "Create bucket here" on its card to start using it.`,
          6,
        );
        setShowForm(false);
        resetForm();
        await refresh();
        requestAnimationFrame(() => {
          document
            .querySelector(`[data-backend-card="${CSS.escape(name)}"]`)
            ?.scrollIntoView({ behavior: 'smooth', block: 'center' });
        });
      } else {
        setFormError(plainError(result.error || '', 'The proxy did not create the backend.'));
      }
    } catch (e) {
      setFormError(plainError(e, 'The proxy did not create the backend.'));
    } finally {
      setSaving(false);
    }
  };

  // The row's "⋯" menu shows this confirmation, then runs removeBackend.
  const removeConfirm = (name: string): RowActionConfirm => {
    const routedHere = countByBackend[name] ?? 0;
    return {
      title: `Remove backend "${name}"?`,
      okText: 'Remove backend',
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
    };
  };
  const removeBackend = async (name: string) => {
    try {
      const result = await deleteBackend(name);
      if (result.success) {
        message.success(`Backend "${name}" removed`);
        await refresh();
      } else {
        setSaveResult({ ok: false, message: plainError(result.error || '', 'The proxy did not remove the backend.').text });
      }
    } catch (e) {
      setSaveResult({ ok: false, message: plainError(e, 'The proxy did not remove the backend.').text });
    }
  };

  // Server-side probe: exercises the SERVER's credentials + endpoint +
  // network (the client-side test never could test creds — the exact class
  // of silent breakage that hid the unpopulated-B2-secrets incident).
  // Updates the server health cache, so the health badge refreshes too.
  const handleTestConnection = async (b: BackendInfo) => {
    setTestingBackend(b.name);
    setTestResult(null);
    try {
      const entry = await probeBackend(b.name);
      setTestResult({
        name: b.name,
        ok: entry.status === 'healthy',
        message: healthLabel(entry),
      });
      qc.invalidateQueries({ queryKey: qk.backends.list() });
    } catch (e) {
      setTestResult({ name: b.name, ok: false, message: plainError(e, 'The connection test failed.').text });
    } finally {
      setTestingBackend(null);
    }
  };

  const resetForm = () => {
    setFormName(''); setFormType('filesystem'); setFormPath(''); setFormError(null); setCreateTried(false);
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
    /** The backend's own mode before the change. */
    prevMode: BackendEncryptionMode,
    patch: BackendEncryptionPatch,
  ): Promise<void> => {
    // Tracks whether the try-block already set a precise result message this
    // run. Using a local (not the closed-over `saveResult` state, which is the
    // stale render snapshot) is what keeps the catch from either suppressing a
    // genuine error — because a PREVIOUS success message is still in state — or
    // clobbering the precise message just set with a generic one.
    let resultSet = false;
    try {
      // The body starts from the storage section itself (fresh GET), not from
      // the backend summaries: the PUT replaces the whole `backends` list, so
      // anything missing from it is removed. If-Match refuses the PUT when
      // another editor changed the section in between.
      const { body: storage, version } = await getSectionVersioned<StorageSectionBackends>('storage');
      const body = buildEncryptionSectionBody(backendName, patch, storage);
      // Turning a backend's encryption off confirms the downgrade for the
      // buckets that inherit from it (`backend:<name>`).
      let confirm: ConfirmHeaders | undefined =
        patch.mode === 'none' && prevMode !== 'none' ? { backendDowngrades: [backendName] } : undefined;
      // Turning on proxy AES over buckets that hold objects starts their
      // conversion on the server: say how many objects, and ask first.
      if (patch.mode === 'aes256-gcm-proxy') {
        const reviewed = await reviewConversions(`Encrypt backend ${backendName}?`, 'Encrypt', body, confirm);
        if (reviewed === null) {
          resultSet = true;
          throw new Error('cancelled');
        }
        confirm = reviewed;
      }
      const result = await applySection(qc, 'storage', body, version, confirm);
      if (!result.ok) {
        setSaveResult({
          ok: false,
          message: result.error || 'Failed to apply encryption change',
        });
        resultSet = true;
        throw new Error(result.error || 'Apply failed');
      }
      const started = result.conversions ?? [];
      setSaveResult({
        ok: true,
        message: [
          `Encryption updated on backend '${backendName}'`,
          started.length > 0
            ? `Re-encrypt jobs encrypt the existing objects of ${started.map((c) => c.bucket).join(', ')} in the background`
            : null,
          persistFailureText(result),
        ]
          .filter(Boolean)
          .join('. '),
      });
      // Existing objects keep their OLD at-rest state. Propose the canned
      // re-encrypt job for the buckets whose effective encryption (the
      // server's answer per bucket) changed: a bucket with its own block is
      // not touched by a backend change, so it is not offered.
      const before = originsQuery.data?.buckets ?? [];
      await refresh();
      const after = (await originsQuery.refetch()).data?.buckets ?? [];
      // A bucket whose conversion the apply started has its job already.
      const converting = new Set(started.map((c) => c.bucket));
      const changed = bucketsWhoseEncryptionChanged(before, after).filter((c) => !converting.has(c.bucket));
      if (changed.length > 0) {
        setReencryptProposal({
          transition: dominantTransition(changed),
          backendName,
          buckets: changed.map((c) => c.bucket),
        });
      }
    } catch (e) {
      if (e instanceof Error && !resultSet) {
        setSaveResult({ ok: false, message: sectionApplyErrorText(e, 'Failed to apply the encryption change') });
      }
      throw e;
    }
  };

  /** Drop a backend's decrypt-only legacy key (the shim banner's action). */
  const handleClearLegacy = async (backendName: string): Promise<void> => {
    try {
      const { body: storage, version } = await getSectionVersioned<StorageSectionBackends>('storage');
      const result = await applySection(qc, 'storage', buildClearLegacySectionBody(backendName, storage), version);
      if (!result.ok) {
        setSaveResult({ ok: false, message: result.error || 'Failed to clear the legacy key' });
        return;
      }
      setSaveResult({ ok: true, message: [`Legacy key cleared on backend '${backendName}'`, persistFailureText(result)].filter(Boolean).join('. ') });
      qc.removeQueries({ queryKey: qk.backends.legacyKeyUsage(backendName) });
      await refresh();
    } catch (e) {
      setSaveResult({ ok: false, message: sectionApplyErrorText(e, 'Failed to clear the legacy key') });
    }
  };

  const globalCompressionOn = (config?.max_delta_ratio ?? 0.75) > 0;
  // DGP_MAX_DELTA_RATIO controls the switch's field: show it read-only.
  const ratioEnv = findEnvOverride(config?.env_overrides, 'advanced.max_delta_ratio');
  // The singleton backend's type/path, when DGP_S3_* or DGP_DATA_DIR sets it.
  const singletonEnv = findEnvOverride(config?.env_overrides, 'storage.backend.type');
  /** The env override for a backend's encryption key or KMS key id, if any. */
  const encryptionEnv = (b: { name: string; is_synthesized?: boolean }) => {
    const name = b.is_synthesized ? null : b.name;
    return (
      findEnvOverride(config?.env_overrides, backendEncryptionEnvPath(name, 'key')) ??
      findEnvOverride(config?.env_overrides, backendEncryptionEnvPath(name, 'kms_key_id'))
    );
  };

  if (loading || listError) {
    return (
      <div style={contentColumn(CONTENT_FORM)}>
        {listError ? (
          <div style={cardStyle}>
            <ErrorState error={listError} what="the backends" onRetry={() => void backendsQuery.refetch()} />
          </div>
        ) : (
          <LoadingState variant="card" rows={4} label="Loading the backends" />
        )}
      </div>
    );
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
      <Space orientation="vertical" size={0} style={{ width: '100%' }}>

        {saveResult && (
          <Alert type={saveResult.ok ? 'success' : 'error'} title={saveResult.message} showIcon closable={{ onClose: () => setSaveResult(null) }} style={{ borderRadius: 8, marginBottom: 12 }} />
        )}

        {/* Storage backends — the page's subject, first. */}
        <div style={cardStyle}>
          <SectionHeader
            icon={<DatabaseOutlined />}
            title="Storage backends"
            description={
              backends.length === 0
                ? 'No backend configured.'
                : backends.every((b) => b.is_synthesized)
                  ? 'The proxy uses one unnamed backend, from storage.backend in the YAML (below). You cannot delete it. When you add a named backend, this one becomes the named backend "default" and keeps its buckets.'
                  : `${backends.length} backend${backends.length !== 1 ? 's' : ''} configured.`
            }
          />

          {backends.map((b) => (
            <BackendCard
              key={b.name}
              name={b.name}
              filesystem={b.backend_type === 'filesystem'}
              kind={backendKind(b)}
              highlighted={b.name === defaultBackend}
              pills={backendPills(b, b.name === defaultBackend, colors)}
              facts={[
                ...backendFacts(b),
                {
                  label: 'Buckets',
                  value: (() => {
                    const n = countByBackend[b.name] ?? 0;
                    return n === 0 ? "None yet. Use 'Create bucket here' to add one." : `${n} routed here`;
                  })(),
                  mono: false,
                },
              ]}
              actions={
                <>
                  <Button
                    size="small"
                    icon={<ApiOutlined />}
                    loading={testingBackend === b.name}
                    onClick={() => handleTestConnection(b)}
                    title="Live server-side probe of this backend's connectivity + credentials"
                  >
                    Test connection
                  </Button>
                  <Button
                    size="small"
                    icon={<PlusOutlined />}
                    onClick={() => setCreateBucketBackend(b.name)}
                    title="Create a bucket on this backend"
                  >
                    Create bucket here
                  </Button>
                </>
              }
              // Remove sits in a "⋯" menu, not as a red icon on every card:
              // a destructive action should take a deliberate second step.
              menu={
                !b.is_synthesized && (
                  <RowActionsMenu
                    label={`More actions for backend ${b.name}`}
                    actions={[
                      {
                        key: 'remove',
                        label: 'Remove backend…',
                        icon: <DeleteOutlined />,
                        danger: true,
                        confirm: removeConfirm(b.name),
                        onSelect: () => removeBackend(b.name),
                      },
                    ]}
                  />
                )
              }
            >
              {b.is_synthesized && singletonEnv && (
                <div data-testid="backend-env-note" style={{ fontSize: 12, color: colors.TEXT_MUTED, marginTop: 8 }}>
                  {envOverrideHelp(singletonEnv).replace('this value', 'this backend')}
                </div>
              )}
              {testResult?.name === b.name && (
                <Alert type={testResult.ok ? 'success' : 'error'} title={testResult.message} showIcon style={{ marginTop: 8, borderRadius: 6 }} />
              )}
              {/* Loud connection-health fault. Only DEFINITIVE faults
                 (credentials rejected / unreachable) gate requests with 503;
                 'erroring' (reachable but 5xx/throttling) still serves and
                 is a warning, not an outage claim. */}
              {b.health && b.health.status !== 'healthy' && (
                <Alert
                  type={b.health.status === 'erroring' ? 'warning' : 'error'}
                  showIcon
                  title={b.health.status === 'erroring' ? 'Backend degraded' : 'Backend unavailable'}
                  description={
                    b.health.status === 'erroring'
                      ? `${healthLabel(b.health)}. Requests are NOT blocked — the backend is reachable but fails some requests. Re-probed every 30s.`
                      : `${healthLabel(b.health)}. Buckets routed to this backend answer 503 until it recovers — re-probed every 30s, or use 'Test connection' after fixing it.`
                  }
                  style={{ marginTop: 8, borderRadius: 6 }}
                />
              )}
              {/* Conditional-write verdict from the startup capability gate
                 (guard B). Absent = not assessed (single-instance, or no
                 client-writable buckets routed here). */}
              {b.capability?.verdict === 'cas-verified' && (
                <Alert
                  type="success"
                  showIcon
                  title="Conditional writes: verified by live probe — safe for multi-instance client writes"
                  style={{ marginTop: 8, borderRadius: 6 }}
                />
              )}
              {b.capability?.verdict === 'non-cas' && (
                <Alert
                  type="warning"
                  showIcon
                  title="Conditional writes NOT supported"
                  description={
                    <LinkifiedText
                      text={
                        'This backend cannot host the coordination bucket or multi-instance ' +
                        'client-writable buckets. It remains a safe replication target: mark its ' +
                        `buckets replication_target_only. — see ${CAPABILITY_DOC_URL}`
                      }
                    />
                  }
                  style={{ marginTop: 8, borderRadius: 6 }}
                />
              )}
              {b.capability?.verdict === 'unknown' && (
                <Alert
                  type="info"
                  showIcon
                  title="Conditional-write support could not be verified"
                  description={
                    <LinkifiedText
                      text={`${b.capability.reason} — multi-instance write safety is unproven on this backend. See ${CAPABILITY_DOC_URL}`}
                    />
                  }
                  style={{ marginTop: 8, borderRadius: 6 }}
                />
              )}
              {/* Per-backend encryption subsection: shows the current
                 mode, exposes a mode-change picker, and wraps the
                 proxy-AES key-generation flow. Apply sends a targeted
                 storage section PUT; siblings preserved by merge-patch. */}
              {encryptionEnv(b) && (
                <Text type="secondary" data-testid="backend-encryption-env-note" style={{ fontSize: 12, display: 'block', marginTop: 8 }}>
                  {envOverrideHelp(encryptionEnv(b)!).replace('this value', "this backend's encryption key")}
                </Text>
              )}
              <BackendEncryptionEditor
                backendName={b.name}
                current={b.encryption}
                onApply={(patch) => handleEncryptionApply(b.name, b.encryption.mode, patch)}
                onClearLegacy={() => handleClearLegacy(b.name)}
                conversions={backendConversions[b.name]}
                readOnly={!!encryptionEnv(b)}
                usage={backendBucketCountText(encryptionCounts[b.name])}
              />
            </BackendCard>
          ))}

          {/* The page's primary action, in the page header (PageActions). */}
          <PageActions>
            <Button data-testid="backends-add" type="primary" icon={<PlusOutlined />} disabled={showForm} onClick={() => setShowForm(true)}>
              New backend
            </Button>
          </PageActions>
        </div>

        {/* New backend form */}
        {showForm && (
          <div style={cardStyle} ref={formRef}>
            <SectionHeader icon={<PlusOutlined />} title="New backend" />
            <div>
              <FormField label="Name" yamlPath="storage.backends[].name">
                <Input data-testid="backend-name" aria-label="Backend name" value={formName} onChange={(e) => setFormName(e.target.value)} placeholder="e.g. local, hetzner, aws-prod" status={createTried && nameMissing ? 'error' : undefined} aria-invalid={createTried && nameMissing} style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                {createTried && nameMissing && (
                  <Text type="danger" style={{ fontSize: 12, display: 'block', marginTop: 4 }}>
                    Enter a name for the backend.
                  </Text>
                )}
              </FormField>
              <FormField label="Type" yamlPath="storage.backends[].type">
                <Radio.Group value={formType} onChange={(e) => setFormType(e.target.value)} style={{ display: 'flex', gap: 0 }}>
                  <Radio.Button value="filesystem" style={{ fontSize: 13 }}>Filesystem</Radio.Button>
                  <Radio.Button value="s3" style={{ fontSize: 13 }}>S3</Radio.Button>
                </Radio.Group>
              </FormField>
              {formType === 'filesystem' && (
                <FormField
                  label="Data directory"
                  yamlPath="storage.backends[].path"
                  helpText="An absolute path on the proxy host, for example /var/lib/deltaglider/data. A relative path would depend on the directory the proxy starts in."
                >
                  <Input
                    data-testid="backend-path"
                    aria-label="Data directory"
                    value={formPath}
                    onChange={(e) => setFormPath(e.target.value)}
                    placeholder="/var/lib/deltaglider/data"
                    status={(formPath.trim() || createTried) && pathInvalid ? 'error' : undefined}
                    aria-invalid={(formPath.trim() || createTried) && pathInvalid ? true : undefined}
                    style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
                  />
                  {(formPath.trim() || createTried) && pathInvalid && (
                    <Text type="danger" style={{ fontSize: 12, display: 'block', marginTop: 4 }}>
                      {ABSOLUTE_PATH_ERROR}
                    </Text>
                  )}
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
                  <div style={{ marginBottom: 24 }}>
                    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                      <Switch id="backend-force-path-style" checked={formForcePathStyle} onChange={setFormForcePathStyle} size="small" />
                      <label htmlFor="backend-force-path-style" style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Force path-style URLs</label>
                    </div>
                    <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 4 }}>
                      Keep ON for S3-compatibles (MinIO, Hetzner, Ceph). Turn OFF for AWS S3.
                    </Text>
                  </div>
                  <FormField label="Access key ID" yamlPath="storage.backends[].access_key_id">
                    <Input value={formAccessKey} onChange={(e) => setFormAccessKey(e.target.value)} placeholder="AKIAIOSFODNN7EXAMPLE" style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }} />
                  </FormField>
                  <FormField label="Secret access key" yamlPath="storage.backends[].secret_access_key">
                    {/* Create-only form: no existing secret to keep, and blank is rejected. */}
                    <MaskedSecretInput
                      mode="new"
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
              <Switch id="backend-set-default" checked={formSetDefault} onChange={setFormSetDefault} size="small" />
              <label htmlFor="backend-set-default" style={{ fontSize: 13, fontFamily: 'var(--font-ui)' }}>Set as default backend</label>
            </div>
            {formError && (
              <PlainErrorAlert error={formError} style={{ marginTop: 16 }} />
            )}
            <div style={{ marginTop: 16, display: 'flex', gap: 8 }}>
              <Button data-testid="backend-create" type="primary" icon={<CheckCircleOutlined />} onClick={handleCreate} loading={saving} style={{ borderRadius: 8, fontWeight: 600 }}>
                Create backend
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
              disabled={ratioEnv !== null}
              onChange={async (on) => {
                try {
                  await updateAdminConfig({ max_delta_ratio: on ? 0.75 : 0 });
                  await refresh();
                } catch (e) {
                  message.error(plainError(e, 'The proxy did not change delta compression.').text);
                }
              }}
            />
            <div>
              <Text style={{ fontSize: 13, fontWeight: 700, fontFamily: 'var(--font-ui)', color: colors.TEXT_PRIMARY }}>
                Delta compression: <span style={{ color: globalCompressionOn ? colors.ACCENT_GREEN : colors.TEXT_SECONDARY }}>{globalCompressionOn ? 'on' : 'off'}</span>
                <span style={{ fontWeight: 400, color: colors.TEXT_MUTED, marginLeft: 8, fontSize: 12 }}>
                  {ratioEnv ? `from env ${envOverrideSource(ratioEnv)}` : 'applies immediately'}
                </span>
              </Text>
              <Text type="secondary" style={{ fontSize: 12, fontFamily: 'var(--font-ui)', display: 'block', marginTop: 2, lineHeight: 1.6 }}>
                {ratioEnv && `${envOverrideHelp(ratioEnv)} `}
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
