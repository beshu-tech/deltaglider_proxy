/**
 * SetupWizard — Wave 8 of the admin UI revamp plan (§8).
 *
 * A five-screen guided onboarding for fresh installs:
 *
 *   1. Pick a storage backend  (filesystem vs S3-compatible)
 *   2. Configure the backend   (path picker OR endpoint/region/creds)
 *   3. Create admin credentials (bootstrap SigV4 key pair)
 *   4. Optional public bucket  (can be skipped)
 *   5. Review + apply          (shows generated YAML; POSTs /config/apply)
 *
 * ## Integration
 *
 * The wizard is a dedicated panel reachable at
 * `/_/admin/setup`. On fresh installs, the Dashboard surfaces a
 * "First time? Start with the setup wizard →" banner so operators
 * are nudged toward it naturally instead of diving straight into
 * the admin nav.
 *
 * Mutations use the existing document-level `/config/apply`
 * endpoint — it already handles full-document application with
 * runtime-secret preservation + persist. The operator must be
 * logged in (session-cookie auth); the wizard does NOT bypass
 * authentication.
 *
 * ## Hard rules (plan §8)
 *
 *   * No screen has more than 3 inputs.
 *   * Every input shows its default (via FormField placeholder).
 *   * Back + Next visible at all times.
 *   * Skip where legitimate.
 *   * Total time under 3 minutes.
 */
import { useState, useEffect, useRef } from 'react';
import {
  Alert,
  Button,
  Input,
  Radio,
  Spin,
  Steps,
  Typography,
  message,
} from 'antd';
import {
  CloudOutlined,
  CloudServerOutlined,
  DatabaseOutlined,
  KeyOutlined,
  CheckCircleOutlined,
  ArrowLeftOutlined,
  ArrowRightOutlined,
  ApiOutlined,
} from '@ant-design/icons';
import {
  applyConfigYaml,
  testS3Connection,
  type TestS3Response,
} from '../adminApi';
import { useColors } from '../ThemeContext';
import { useCopyToClipboard } from '../useCopyToClipboard';
import { useCardStyles, contentColumn, CONTENT_FORM } from './shared-styles';
import FormField from './FormField';
import { normalizeUiError } from '../errorHandling';
import { useNavigation } from '../NavigationContext';
import { parseAdminQuery, buildViewUrl } from '../urlState';
import {
  generateSetupYaml,
  setupEnvControl,
  setupStepComplete,
  type SetupEnvControl,
} from '../setupYaml';
import { useAdminConfig } from '../queries/config';
import { describeExistingSetup, type ExistingSetup } from '../setupDetect';
import { ABSOLUTE_PATH_ERROR, isAbsolutePath } from '../utils';

const { Text, Paragraph } = Typography;

/** "2 named backends, 5 bucket settings and 1 request rule" (non-zero parts only). */
function describeCounts(e: ExistingSetup): string {
  const n = (count: number, one: string, many: string) => `${count} ${count === 1 ? one : many}`;
  const parts = [
    e.backendCount > 0 ? n(e.backendCount, 'named backend', 'named backends') : '',
    e.bucketSettingsCount > 0 ? n(e.bucketSettingsCount, 'bucket setting', 'bucket settings') : '',
    e.requestRuleCount > 0 ? n(e.requestRuleCount, 'request rule', 'request rules') : '',
  ].filter(Boolean);
  return parts.length > 1 ? `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}` : parts[0] ?? '';
}

type BackendKind = 'filesystem' | 's3';

interface WizardState {
  backendKind: BackendKind;
  // Filesystem
  fsPath: string;
  // S3
  s3Endpoint: string;
  s3Region: string;
  s3ForcePathStyle: boolean;
  s3AccessKey: string;
  s3SecretKey: string;
  // Admin credentials
  adminAccessKeyId: string;
  adminSecretKey: string;
  adminSecretKeyConfirm: string;
  // Optional public bucket
  publicBucketName: string;
  enablePublicBucket: boolean;
}

const INITIAL: WizardState = {
  backendKind: 'filesystem',
  // No prefill: a relative path such as ./data depends on the directory the
  // proxy starts in (systemd, Docker and a shell differ). Must be absolute.
  fsPath: '',
  s3Endpoint: '',
  s3Region: 'us-east-1',
  s3ForcePathStyle: true,
  s3AccessKey: '',
  s3SecretKey: '',
  adminAccessKeyId: '',
  adminSecretKey: '',
  adminSecretKeyConfirm: '',
  publicBucketName: '',
  enablePublicBucket: false,
};

/** Last valid wizard step index (0-based across the 5 screens). */
const MAX_STEP = 4;

interface Props {
  onComplete: () => void;
  onCancel: () => void;
  /** Raw query string (with leading `?`) for the ?step=N deep-link. */
  search?: string;
}

export default function SetupWizard({ onComplete, onCancel, search }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();
  const { navigate } = useNavigation();

  const [state, setState] = useState<WizardState>(INITIAL);

  // The wizard applies a WHOLE configuration document. On a proxy that is
  // already configured, say so first, and start from the running backend's
  // type instead of a Filesystem default that contradicts it.
  // Only the proxy's own configuration counts (named backends, bucket
  // settings, request rules), not buckets that exist on the storage.
  // Until the check settles the steps stay hidden; if it fails, the
  // warning shows (fail closed).
  const configQuery = useAdminConfig();
  const existing = configQuery.data
    ? describeExistingSetup(
        configQuery.data.backends,
        {
          bucketSettings: Object.keys(configQuery.data.bucket_policies ?? {}).length,
          requestRules: configQuery.data.admission_blocks?.length ?? 0,
        },
        configQuery.data.default_backend,
      )
    : null;
  const checkFailed = configQuery.isError && !configQuery.data;
  const [overwriteConfirmed, setOverwriteConfirmed] = useState(false);
  // Answers the environment already gives (DGP_* variables): shown
  // read-only, counted as answered, and left out of the generated YAML.
  const envControl = setupEnvControl(configQuery.data?.env_overrides);
  const envBackendKind = envControl.backendKind;
  useEffect(() => {
    if (envBackendKind) setState((s) => ({ ...s, backendKind: envBackendKind }));
  }, [envBackendKind]);
  const seeded = useRef(false);
  useEffect(() => {
    if (!existing || seeded.current) return;
    seeded.current = true;
    if (!existing.kind) return;
    setState((s) => ({
      ...s,
      backendKind: existing.kind ?? s.backendKind,
      fsPath: existing.fsPath && isAbsolutePath(existing.fsPath) ? existing.fsPath : s.fsPath,
      s3Endpoint: existing.s3Endpoint,
      s3Region: existing.s3Region,
      s3ForcePathStyle: existing.s3ForcePathStyle,
    }));
  }, [existing]);
  // The wizard step is deep-linked via ?step=N so browser Back traverses
  // steps in-session. But we DON'T restore from URL on initial load — a
  // refresh resets form state to INITIAL while the URL still says ?step=4,
  // which would land the operator on the Review step over empty config.
  // The URL sync effect below handles in-session Back/Forward only.
  const [step, setStep] = useState(0);
  const stepSyncMounted = useRef(false);
  // Keep local state in sync with the URL so browser Back/Forward
  // (which changes `search` without touching the wizard buttons) still
  // moves the displayed step. Skip the first run so a direct-loaded
  // ?step=N doesn't override the safe initial step 0.
  useEffect(() => {
    if (!stepSyncMounted.current) {
      stepSyncMounted.current = true;
      return;
    }
    const q = parseAdminQuery(search || '');
    const n = parseInt(q.step ?? '', 10);
    setStep(Number.isNaN(n) ? 0 : Math.max(0, Math.min(n, MAX_STEP)));
  }, [search]);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestS3Response | null>(null);
  const [applying, setApplying] = useState(false);

  // Any edit to an S3 connection field invalidates a prior "Connected ✓" —
  // otherwise canAdvance (step 1) would let the operator proceed past an
  // untested config change on the strength of a now-stale successful test.
  const S3_TEST_FIELDS: (keyof WizardState)[] = [
    's3Endpoint',
    's3Region',
    's3AccessKey',
    's3SecretKey',
  ];
  const update = (patch: Partial<WizardState>) => {
    setState((s) => ({ ...s, ...patch }));
    if (S3_TEST_FIELDS.some((f) => f in patch)) {
      setTestResult(null);
    }
  };

  const generatedYaml = generateSetupYaml(state, envControl);

  // Validation per step (pure, see setupYaml.ts). An S3 backend needs a
  // passed connection test unless the environment sets the backend.
  const canAdvance = setupStepComplete(
    step,
    state,
    envControl,
    testResult?.success === true,
    isAbsolutePath,
  );

  const next = () => {
    const ns = Math.min(step + 1, MAX_STEP);
    setStep(ns);
    // Push (not replace) so browser Back walks through wizard steps
    // rather than exiting the flow on the first press.
    navigate(buildViewUrl('admin', 'setup', { step: String(ns) }));
  };
  const prev = () => {
    const ns = Math.max(step - 1, 0);
    setStep(ns);
    navigate(buildViewUrl('admin', 'setup', { step: String(ns) }));
  };

  const runTest = async () => {
    setTesting(true);
    setTestResult(null);
    try {
      const result = await testS3Connection({
        endpoint: state.s3Endpoint || undefined,
        region: state.s3Region,
        force_path_style: state.s3ForcePathStyle,
        access_key_id: state.s3AccessKey || undefined,
        secret_access_key: state.s3SecretKey || undefined,
      });
      setTestResult(result);
    } finally {
      setTesting(false);
    }
  };

  const apply = async () => {
    setApplying(true);
    try {
      const resp = await applyConfigYaml(generatedYaml);
      if (!resp.applied) {
        message.error(resp.error || 'Apply failed');
        return;
      }
      if (resp.warnings && resp.warnings.length > 0) {
        message.warning(`Applied with ${resp.warnings.length} warning(s)`);
      } else {
        message.success('Setup complete!');
      }
      onComplete();
    } catch (e) {
      message.error(
        `Apply failed: ${normalizeUiError(e, 'unknown')}`);
    } finally {
      setApplying(false);
    }
  };

  const screen = (() => {
    switch (step) {
      case 0:
        return (
          <PickBackendStep
            envKind={envControl.backendKind}
            value={state.backendKind}
            onChange={(kind) => {
              update({ backendKind: kind });
              // Reset test result when switching backend kind so the
              // operator doesn't carry a stale "Connected ✓" into a
              // different backend config.
              setTestResult(null);
            }}
          />
        );
      case 1:
        return (
          <ConfigureBackendStep
            env={envControl}
            state={state}
            update={update}
            testing={testing}
            testResult={testResult}
            onTest={runTest}
            cardStyle={cardStyle}
            inputRadius={inputRadius}
          />
        );
      case 2:
        return (
          <CreateAdminStep
            env={envControl}
            state={state}
            update={update}
            cardStyle={cardStyle}
            inputRadius={inputRadius}
          />
        );
      case 3:
        return (
          <OptionalPublicBucketStep
            state={state}
            update={update}
            cardStyle={cardStyle}
            inputRadius={inputRadius}
          />
        );
      case 4:
        return <ReviewStep yaml={generatedYaml} cardStyle={cardStyle} />;
      default:
        return null;
    }
  })();

  return (
    <div
      style={{
        ...contentColumn(CONTENT_FORM),
        padding: 'clamp(16px, 4vw, 32px)',
        display: 'flex',
        flexDirection: 'column',
        gap: 24,
      }}
    >
      {/* Hero */}
      <header style={{ marginBottom: 8 }}>
        <Text
          style={{
            fontSize: 11,
            fontWeight: 700,
            letterSpacing: 1.5,
            textTransform: 'uppercase',
            color: colors.ACCENT_BLUE,
            fontFamily: 'var(--font-ui)',
            display: 'block',
            marginBottom: 6,
          }}
        >
          First-run setup
        </Text>
        <h2
          style={{
            margin: 0,
            fontSize: 28,
            fontWeight: 700,
            fontFamily: 'var(--font-ui)',
            color: colors.TEXT_PRIMARY,
            letterSpacing: '-0.01em',
          }}
        >
          Get DeltaGlider Proxy running in under 3 minutes.
        </h2>
        <Paragraph
          type="secondary"
          style={{
            fontSize: 14,
            marginTop: 6,
            marginBottom: 0,
            lineHeight: 1.5,
          }}
        >
          Five quick questions. Every answer is editable later via the
          admin GUI or by hand-editing your YAML.
        </Paragraph>
      </header>

      {configQuery.isPending ? (
        <div style={{ display: 'flex', justifyContent: 'center', padding: 48 }}>
          <Spin description="Checking the current configuration…"><div style={{ padding: 24 }} /></Spin>
        </div>
      ) : (existing?.configured || checkFailed) && !overwriteConfirmed ? (
        <Alert
          type="warning"
          showIcon
          title={checkFailed ? 'Could not check the current configuration' : 'This proxy is already configured'}
          description={
            <div>
              <Paragraph style={{ marginBottom: 12 }}>
                {existing ? (
                  <>It has {describeCounts(existing)}. </>
                ) : (
                  <>{normalizeUiError(configQuery.error, 'The configuration did not load').replace(/\.$/, '')}. The proxy may already be configured. </>
                )}
                This wizard writes a complete new configuration, which replaces the current one:
                backends, bucket settings and request rules that the wizard does not ask about are
                removed. The objects in your buckets are not touched. To change one thing, use its
                settings page instead.
              </Paragraph>
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <Button type="primary" onClick={onCancel}>
                  Back to the dashboard
                </Button>
                <Button onClick={() => setOverwriteConfirmed(true)}>
                  Start over anyway
                </Button>
              </div>
            </div>
          }
        />
      ) : (
      <>
      <Steps
        current={step}
        size="small"
        items={[
          { title: 'Backend', icon: <DatabaseOutlined /> },
          { title: 'Configure', icon: <CloudServerOutlined /> },
          { title: 'Admin', icon: <KeyOutlined /> },
          { title: 'Public?', icon: <CloudOutlined /> },
          { title: 'Review', icon: <CheckCircleOutlined /> },
        ]}
      />

      <div style={{ minHeight: 320 }}>{screen}</div>

      {/* Nav */}
      <div
        style={{
          display: 'flex',
          justifyContent: 'space-between',
          alignItems: 'center',
          paddingTop: 12,
          borderTop: `1px solid ${colors.BORDER}`,
        }}
      >
        <Button
          icon={<ArrowLeftOutlined />}
          onClick={step === 0 ? onCancel : prev}
          disabled={applying}
        >
          {step === 0 ? 'Cancel' : 'Back'}
        </Button>
        <Text type="secondary" style={{ fontSize: 12 }}>
          Step {step + 1} of 5
        </Text>
        {step < 4 ? (
          <Button
            type="primary"
            icon={<ArrowRightOutlined />}
            onClick={next}
            disabled={!canAdvance}
          >
            Next
          </Button>
        ) : (
          <Button
            type="primary"
            icon={<CheckCircleOutlined />}
            onClick={apply}
            loading={applying}
          >
            Save and start
          </Button>
        )}
      </div>
      </>
      )}
    </div>
  );
}

// ───────────────────────────────────────────────────────────
// Steps
// ───────────────────────────────────────────────────────────

function PickBackendStep({
  envKind,
  value,
  onChange,
}: {
  envKind: BackendKind | null;
  value: BackendKind;
  onChange: (kind: BackendKind) => void;
}) {
  const colors = useColors();
  const { cardStyle } = useCardStyles();
  return (
    <div style={cardStyle}>
      <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)', fontSize: 18 }}>
        Where should data live?
      </h3>
      <Text
        type="secondary"
        style={{ fontSize: 13, display: 'block', marginTop: 4, marginBottom: 20 }}
      >
        This proxy needs a backing store. Pick the one that matches your
        infrastructure. You can always change this later.
      </Text>
      {envKind && (
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 16, borderRadius: 8 }}
          title={`Environment variables set this proxy's backend (${envKind === 's3' ? 'S3-compatible' : 'filesystem'}). To choose another one, change them and restart the proxy.`}
        />
      )}
      <Radio.Group
        value={value}
        disabled={envKind !== null}
        onChange={(e) => onChange(e.target.value)}
        style={{ display: 'flex', flexDirection: 'column', gap: 14 }}
      >
        <Radio value="filesystem" style={{ alignItems: 'flex-start' }}>
          <div>
            <div style={{ fontWeight: 600, fontSize: 15 }}>
              Filesystem{' '}
              <span style={{ fontSize: 11, color: colors.TEXT_MUTED, fontWeight: 500 }}>
                (good for homelab + local development)
              </span>
            </div>
            <Text type="secondary" style={{ fontSize: 12, marginTop: 2, display: 'block' }}>
              Store objects as files on a local directory. Zero external
              dependencies — start and you're running.
            </Text>
          </div>
        </Radio>
        <Radio value="s3" style={{ alignItems: 'flex-start' }}>
          <div>
            <div style={{ fontWeight: 600, fontSize: 15 }}>
              S3-compatible{' '}
              <span style={{ fontSize: 11, color: colors.TEXT_MUTED, fontWeight: 500 }}>
                (recommended for production)
              </span>
            </div>
            <Text type="secondary" style={{ fontSize: 12, marginTop: 2, display: 'block' }}>
              Target AWS S3, MinIO, Hetzner Object Storage, Backblaze,
              Cloudflare R2, or any SigV4-compatible endpoint. This proxy
              becomes a delta-compression + access-control layer on top.
            </Text>
          </div>
        </Radio>
      </Radio.Group>
    </div>
  );
}

function ConfigureBackendStep({
  env,
  state,
  update,
  testing,
  testResult,
  onTest,
  cardStyle,
  inputRadius,
}: {
  env: SetupEnvControl;
  state: WizardState;
  update: (patch: Partial<WizardState>) => void;
  testing: boolean;
  testResult: TestS3Response | null;
  onTest: () => void;
  cardStyle: React.CSSProperties;
  inputRadius: { borderRadius: number };
}) {
  if (state.backendKind === 'filesystem') {
    return (
      <div style={cardStyle}>
        <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)', fontSize: 18 }}>
          Pick a data directory
        </h3>
        <Text type="secondary" style={{ fontSize: 13, display: 'block', marginTop: 4, marginBottom: 16 }}>
          {env.backendKind !== null
            ? 'The environment sets this directory. Its value is shown below.'
            : 'Objects live under this path. Use an absolute path: a relative one would depend on the directory the proxy starts in. Make sure it is on a disk with enough room.'}
        </Text>
        <FormField
          label="Data directory"
          yamlPath="storage.backend.path"
          helpText="Must be writable by the proxy user."
          examples={['/var/lib/deltaglider', '/mnt/fast-nvme/dgp']}
          onExampleClick={(v) => update({ fsPath: String(v) })}
        >
          <Input
            aria-label="Data directory"
            value={state.fsPath}
            onChange={(e) => update({ fsPath: e.target.value })}
            placeholder="/var/lib/deltaglider"
            status={state.fsPath.trim() && !isAbsolutePath(state.fsPath) ? 'error' : undefined}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
          {state.fsPath.trim() && !isAbsolutePath(state.fsPath) && (
            <Text type="danger" style={{ fontSize: 12, display: 'block', marginTop: 4 }}>
              {ABSOLUTE_PATH_ERROR}
            </Text>
          )}
        </FormField>
      </div>
    );
  }

  // S3
  return (
    <div style={cardStyle}>
      <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)', fontSize: 18 }}>
        Connect to your S3-compatible store
      </h3>
      <Text type="secondary" style={{ fontSize: 13, display: 'block', marginTop: 4, marginBottom: 16 }}>
        {env.backendKind !== null
          ? 'Environment variables connect this proxy to its S3 store. Their values are shown below.'
          : "Enter the endpoint and credentials. Test the connection before you continue: you can't go on until the test passes, because a typo is easier to catch now than later."}
      </Text>
      <FormField
        label="Endpoint URL"
        yamlPath="storage.backend.endpoint"
        helpText={env.backendKind !== null ? undefined : 'Leave empty for AWS default. Include http(s):// scheme.'}
        examples={[
          'https://s3.us-east-1.amazonaws.com',
          'http://localhost:9000',
          'https://hel1.your-objectstorage.com',
        ]}
        onExampleClick={(v) => update({ s3Endpoint: String(v) })}
      >
        <Input
          value={state.s3Endpoint}
          onChange={(e) => update({ s3Endpoint: e.target.value })}
          placeholder="(empty = AWS S3 default)"
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
        />
      </FormField>
      <FormField
        label="Region"
        yamlPath="storage.backend.region"
        helpText="Region of the S3 endpoint. Required by AWS SigV4; many S3-compatible stores accept any value."
        examples={['us-east-1', 'eu-central-1', 'hel1']}
        onExampleClick={(v) => update({ s3Region: String(v) })}
      >
        <Input
          value={state.s3Region}
          onChange={(e) => update({ s3Region: e.target.value })}
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
        />
      </FormField>
      <div style={{ display: 'flex', flexWrap: 'wrap', columnGap: 12 }}>
        <FormField
          style={{ flex: '1 1 220px', minWidth: 0 }}
          label="Access key ID"
          yamlPath="storage.backend.access_key_id"
          helpText="Access key the proxy uses to reach the upstream S3 backend."
        >
          <Input
            value={state.s3AccessKey}
            onChange={(e) => update({ s3AccessKey: e.target.value })}
            placeholder="AKIA..."
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
        </FormField>
        <FormField
          style={{ flex: '1 1 220px', minWidth: 0 }}
          label="Secret access key"
          yamlPath="storage.backend.secret_access_key"
          helpText="Secret paired with the access key ID. Stored in the backend config; never exposed to S3 clients."
        >
          <Input.Password
            value={state.s3SecretKey}
            onChange={(e) => update({ s3SecretKey: e.target.value })}
            autoComplete="new-password"
            style={{ ...inputRadius }}
          />
        </FormField>
      </div>
      {env.backendKind !== null ? (
        <Text type="secondary" style={{ fontSize: 13, display: 'block' }}>
          Environment variables give these settings, so there is nothing to
          enter or test here.
        </Text>
      ) : (
      <Button
        icon={<ApiOutlined />}
        onClick={onTest}
        loading={testing}
        disabled={!state.s3AccessKey || !state.s3SecretKey}
        style={{ marginTop: 12, borderRadius: 8 }}
        block
      >
        Test connection
      </Button>
      )}
      {testResult && env.backendKind === null && (
        <Alert
          type={testResult.success ? 'success' : 'error'}
          showIcon
          style={{ marginTop: 12, borderRadius: 8 }}
          title={
            testResult.success
              ? `Connected — ${testResult.buckets?.length ?? 0} bucket(s) visible`
              : 'Connection failed'
          }
          description={
            testResult.success
              ? testResult.buckets?.slice(0, 10).join(', ') || 'No buckets yet — that\'s fine; we\'ll create them as needed.'
              : testResult.error
          }
        />
      )}
    </div>
  );
}

function CreateAdminStep({
  env,
  state,
  update,
  cardStyle,
  inputRadius,
}: {
  env: SetupEnvControl;
  state: WizardState;
  update: (patch: Partial<WizardState>) => void;
  cardStyle: React.CSSProperties;
  inputRadius: { borderRadius: number };
}) {
  const passwordsMatch =
    state.adminSecretKey.length === 0 ||
    state.adminSecretKey === state.adminSecretKeyConfirm;
  return (
    <div style={cardStyle}>
      <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)', fontSize: 18 }}>
        Create your admin credentials
      </h3>
      <Text type="secondary" style={{ fontSize: 13, display: 'block', marginTop: 4, marginBottom: 16 }}>
        Bootstrap SigV4 credentials that S3 clients (including this browser)
        use to sign requests. These are the "first admin" — additional IAM
        users get created from the Users panel later.
      </Text>
      <FormField
        label="Access key ID"
        yamlPath="access.access_key_id"
        // Input rules describe the input; an env-controlled field has none.
        helpText={env.adminAccessKeyId ? undefined : '4+ characters. Convention: uppercase, starts with AKIA.'}
        examples={['AKIAADMINDEVELOPER01', 'AKIAMYDGPSITEADMIN']}
        onExampleClick={(v) => update({ adminAccessKeyId: String(v) })}
      >
        <Input
          value={state.adminAccessKeyId}
          onChange={(e) => update({ adminAccessKeyId: e.target.value })}
          placeholder="AKIA..."
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
        />
      </FormField>
      <FormField
        label="Secret access key"
        yamlPath="access.secret_access_key"
        helpText={
          env.adminSecretKey
            ? undefined
            : "At least 12 characters. Use a password manager; you won't see this again after Save."
        }
      >
        <Input.Password
          value={state.adminSecretKey}
          onChange={(e) => update({ adminSecretKey: e.target.value })}
          autoComplete="new-password"
          style={{ ...inputRadius }}
        />
      </FormField>
      {!env.adminSecretKey && (
      <FormField label="Confirm secret access key" helpText="Paste it again to catch typos.">
        <Input.Password
          value={state.adminSecretKeyConfirm}
          onChange={(e) => update({ adminSecretKeyConfirm: e.target.value })}
          autoComplete="new-password"
          status={!passwordsMatch ? 'error' : undefined}
          style={{ ...inputRadius }}
        />
        {!passwordsMatch && (
          <Text type="danger" style={{ fontSize: 12 }}>
            Secret keys don't match.
          </Text>
        )}
      </FormField>
      )}
    </div>
  );
}

function OptionalPublicBucketStep({
  state,
  update,
  cardStyle,
  inputRadius,
}: {
  state: WizardState;
  update: (patch: Partial<WizardState>) => void;
  cardStyle: React.CSSProperties;
  inputRadius: { borderRadius: number };
}) {
  return (
    <div style={cardStyle}>
      <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)', fontSize: 18 }}>
        A public bucket? <Text type="secondary" style={{ fontSize: 15, fontWeight: 400 }}>(optional)</Text>
      </h3>
      <Text type="secondary" style={{ fontSize: 13, display: 'block', marginTop: 4, marginBottom: 16 }}>
        Do you want to let anyone read one of your buckets anonymously?
        Typical cases: a docs site, a public release feed, a CDN origin.
        You can skip this and add public buckets later from Storage → Buckets.
      </Text>
      <Radio.Group
        value={state.enablePublicBucket ? 'yes' : 'no'}
        onChange={(e) => update({ enablePublicBucket: e.target.value === 'yes' })}
        style={{ display: 'flex', flexDirection: 'column', gap: 10, marginBottom: 16 }}
      >
        <Radio value="no">
          <span style={{ fontWeight: 600 }}>No, skip this step</span>
          <Text type="secondary" style={{ fontSize: 12, display: 'block' }}>
            All buckets stay authenticated-only.
          </Text>
        </Radio>
        <Radio value="yes">
          <span style={{ fontWeight: 600 }}>Yes, set up one public bucket now</span>
          <Text type="secondary" style={{ fontSize: 12, display: 'block' }}>
            Anyone can download objects from this bucket without credentials.
          </Text>
        </Radio>
      </Radio.Group>
      {state.enablePublicBucket && (
        <FormField
          label="Bucket name"
          yamlPath="storage.buckets.<name>"
          helpText="Lowercase letters, digits, dots, and hyphens only. The wizard makes this bucket publicly readable."
          examples={['docs', 'releases', 'public-assets']}
          onExampleClick={(v) => update({ publicBucketName: String(v) })}
        >
          <Input
            value={state.publicBucketName}
            onChange={(e) =>
              update({
                publicBucketName: e.target.value.toLowerCase().replace(/[^a-z0-9.-]/g, ''),
              })
            }
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
        </FormField>
      )}
    </div>
  );
}

function ReviewStep({ yaml, cardStyle }: { yaml: string; cardStyle: React.CSSProperties }) {
  const colors = useColors();
  const { copy, copied } = useCopyToClipboard();
  return (
    <div style={cardStyle}>
      <h3 style={{ margin: 0, fontFamily: 'var(--font-ui)', fontSize: 18 }}>
        Review + apply
      </h3>
      <Text type="secondary" style={{ fontSize: 13, display: 'block', marginTop: 4, marginBottom: 16 }}>
        This is the YAML that the wizard will apply. Save it somewhere safe
        for GitOps — the admin GUI can regenerate it any time via Export YAML.
      </Text>
      <div style={{ position: 'relative' }}>
        <Button
          size="small"
          onClick={() => {
            void copy(yaml, { successMessage: 'Copied setup YAML to clipboard', resetMs: 1500 });
          }}
          style={{
            position: 'absolute',
            top: 8,
            right: 8,
            zIndex: 1,
            borderRadius: 6,
          }}
        >
          {copied ? 'Copied!' : 'Copy'}
        </Button>
        <pre
          style={{
            margin: 0,
            padding: 16,
            background: colors.BG_ELEVATED,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 10,
            fontFamily: 'var(--font-mono)',
            fontSize: 12,
            lineHeight: 1.6,
            color: colors.TEXT_SECONDARY,
            maxHeight: 400,
            overflowY: 'auto',
          }}
        >
          {yaml}
        </pre>
      </div>
      <Alert
        type="info"
        showIcon
        style={{ marginTop: 12, borderRadius: 8 }}
        title="What happens next"
        description={
          <ul style={{ margin: 0, paddingLeft: 20, fontSize: 13, lineHeight: 1.6 }}>
            <li>The YAML is applied in-memory and persisted to disk.</li>
            <li>S3 clients can immediately sign with the bootstrap key you set.</li>
            <li>You land on the admin Dashboard.</li>
          </ul>
        }
      />
    </div>
  );
}
