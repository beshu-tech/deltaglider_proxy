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
import { useState } from 'react';
import {
  Alert,
  Button,
  Input,
  Radio,
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
import { useCardStyles } from './shared-styles';
import FormField from './FormField';

const { Text, Paragraph } = Typography;

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
  fsPath: './data',
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

interface Props {
  onComplete: () => void;
  onCancel: () => void;
}

export default function SetupWizard({ onComplete, onCancel }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();

  const [state, setState] = useState<WizardState>(INITIAL);
  const [step, setStep] = useState(0);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<TestS3Response | null>(null);
  const [applying, setApplying] = useState(false);

  const update = (patch: Partial<WizardState>) =>
    setState((s) => ({ ...s, ...patch }));

  const generatedYaml = generateYaml(state);

  // Validation per step.
  const canAdvance = (() => {
    switch (step) {
      case 0:
        return true;
      case 1:
        if (state.backendKind === 'filesystem') {
          return state.fsPath.trim().length > 0;
        }
        // S3: connection test must have succeeded.
        return testResult?.success === true;
      case 2:
        return (
          state.adminAccessKeyId.trim().length >= 4 &&
          state.adminSecretKey.length >= 12 &&
          state.adminSecretKey === state.adminSecretKeyConfirm
        );
      case 3:
        return true; // optional
      case 4:
        return true;
      default:
        return false;
    }
  })();

  const next = () => setStep((s) => Math.min(s + 1, 4));
  const prev = () => setStep((s) => Math.max(s - 1, 0));

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
        `Apply failed: ${e instanceof Error ? e.message : 'unknown'}`
      );
    } finally {
      setApplying(false);
    }
  };

  const screen = (() => {
    switch (step) {
      case 0:
        return (
          <PickBackendStep
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
        maxWidth: 820,
        margin: '0 auto',
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
    </div>
  );
}

// ───────────────────────────────────────────────────────────
// Steps
// ───────────────────────────────────────────────────────────

function PickBackendStep({
  value,
  onChange,
}: {
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
      <Radio.Group
        value={value}
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
  state,
  update,
  testing,
  testResult,
  onTest,
  cardStyle,
  inputRadius,
}: {
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
          Objects live under this path. Relative paths resolve from the proxy's
          working directory. Make sure it's on a disk with enough room.
        </Text>
        <FormField
          label="Data directory"
          yamlPath="storage.backend.path"
          helpText="Must be writable by the proxy user."
          examples={['./data', '/var/lib/deltaglider', '/mnt/fast-nvme/dgp']}
          onExampleClick={(v) => update({ fsPath: String(v) })}
        >
          <Input
            value={state.fsPath}
            onChange={(e) => update({ fsPath: e.target.value })}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
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
        Enter the endpoint + credentials. Test Connection proves the whole
        loop works before you commit. You can't advance until the test
        passes — catching a typo now beats debugging later.
      </Text>
      <FormField
        label="Endpoint URL"
        yamlPath="storage.backend.endpoint"
        helpText="Leave empty for AWS default. Include http(s):// scheme."
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
        examples={['us-east-1', 'eu-central-1', 'hel1']}
        onExampleClick={(v) => update({ s3Region: String(v) })}
      >
        <Input
          value={state.s3Region}
          onChange={(e) => update({ s3Region: e.target.value })}
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
        />
      </FormField>
      <div style={{ display: 'flex', gap: 12 }}>
        <FormField
          label="Access key ID"
          yamlPath="storage.backend.access_key_id"
        >
          <Input
            value={state.s3AccessKey}
            onChange={(e) => update({ s3AccessKey: e.target.value })}
            placeholder="AKIA..."
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
        </FormField>
        <FormField
          label="Secret access key"
          yamlPath="storage.backend.secret_access_key"
        >
          <Input.Password
            value={state.s3SecretKey}
            onChange={(e) => update({ s3SecretKey: e.target.value })}
            autoComplete="new-password"
            style={{ ...inputRadius }}
          />
        </FormField>
      </div>
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
      {testResult && (
        <Alert
          type={testResult.success ? 'success' : 'error'}
          showIcon
          style={{ marginTop: 12, borderRadius: 8 }}
          message={
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
        helpText="4+ characters. Convention: uppercase, starts with AKIA."
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
        helpText="At least 12 characters. Use a password manager; you won't see this again after Save."
      >
        <Input.Password
          value={state.adminSecretKey}
          onChange={(e) => update({ adminSecretKey: e.target.value })}
          autoComplete="new-password"
          style={{ ...inputRadius }}
        />
      </FormField>
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
            Anyone can GET objects from this bucket without SigV4 signing.
          </Text>
        </Radio>
      </Radio.Group>
      {state.enablePublicBucket && (
        <FormField
          label="Bucket name"
          yamlPath="storage.buckets.<name>"
          helpText="Lowercase, alphanumerics + dots + hyphens only. The wizard will set `public: true` on this bucket."
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
  const [copied, setCopied] = useState(false);
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
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(yaml);
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            } catch {
              /* noop */
            }
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
        message="What happens next"
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

// ───────────────────────────────────────────────────────────
// YAML generation
// ───────────────────────────────────────────────────────────

/** Build the YAML document from the wizard state. Emits the
 *  canonical four-section shape. Fields at their default are
 *  omitted so the produced file is small and diff-friendly. */
function generateYaml(s: WizardState): string {
  const lines: string[] = [];
  lines.push('# Generated by the DeltaGlider Proxy setup wizard.');
  lines.push('# Re-run the wizard any time from /_/admin/setup or edit fields below by hand.');
  lines.push('');

  // ── access ─────────────────────────────────────────────
  if (s.adminAccessKeyId || s.adminSecretKey) {
    lines.push('access:');
    if (s.adminAccessKeyId) {
      lines.push(`  access_key_id: "${s.adminAccessKeyId}"`);
    }
    if (s.adminSecretKey) {
      lines.push(`  secret_access_key: "${s.adminSecretKey}"`);
    }
    lines.push('');
  }

  // ── storage ────────────────────────────────────────────
  lines.push('storage:');
  if (s.backendKind === 'filesystem') {
    // Use the shorthand form when region/creds aren't needed.
    lines.push(`  filesystem: "${s.fsPath}"`);
  } else {
    // S3 — use the shorthand form when feasible, full form otherwise.
    lines.push(`  s3: "${s.s3Endpoint}"`);
    if (s.s3Region && s.s3Region !== 'us-east-1') {
      lines.push(`  region: "${s.s3Region}"`);
    }
    if (s.s3AccessKey) {
      lines.push(`  access_key_id: "${s.s3AccessKey}"`);
    }
    if (s.s3SecretKey) {
      lines.push(`  secret_access_key: "${s.s3SecretKey}"`);
    }
    if (!s.s3ForcePathStyle) {
      lines.push(`  force_path_style: false`);
    }
  }
  if (s.enablePublicBucket && s.publicBucketName.trim()) {
    lines.push('  buckets:');
    lines.push(`    ${s.publicBucketName.trim()}:`);
    lines.push('      public: true');
  }
  lines.push('');

  return lines.join('\n');
}
