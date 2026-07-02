/**
 * TracePanel — Wave 9.2 of the admin UI revamp.
 *
 * Synthetic-request debugger for the admission chain. Operator
 * enters the request shape (method, path, source IP, auth state,
 * optional query string), we POST it to `/_/api/admin/config/trace`,
 * and render the layer-by-layer decision.
 *
 * Operator mental model: "show me which block would fire for
 * this request and why." First debug step on every support
 * ticket — matches what the CLI `deltaglider_proxy admission
 * trace` will do (Phase 4 of the config refactor).
 *
 * The "reason path" rendering is stolen from Istio/Kiali's
 * validation-trace UX:
 *
 *   GET /releases/builds/v1.zip from 203.0.113.5
 *     → admission: deny-known-bad-ips matched (source_ip_list)
 *     → action: deny (403 Forbidden)
 *
 * Clear breadcrumb, no JSON dump, names the block + the field
 * inside the match that fired.
 */
import { useState } from 'react';
import {
  Alert,
  Button,
  Input,
  Radio,
  Space,
  Switch,
  Tag,
  Typography,
  message,
} from 'antd';
import {
  ExperimentOutlined,
  CheckCircleOutlined,
  CloseCircleOutlined,
} from '@ant-design/icons';
import { adminFetch } from '../adminApi';
import { throwApiError, normalizeUiError } from '../errorHandling';
import { buildTraceBody } from '../traceRequest';
import { useColors } from '../ThemeContext';
import { useCardStyles, contentColumn, CONTENT_FORM } from './shared-styles';
import { METHODS } from '../schemas/admissionSchema';
import SectionHeader from './SectionHeader';
import FormField from './FormField';

const { Text, Paragraph } = Typography;

type Method = (typeof METHODS)[number];

interface TraceResolved {
  method: string;
  bucket: string;
  key: string | null;
  list_prefix: string | null;
  authenticated: boolean;
}

interface TraceDecision {
  decision: 'allow-anonymous' | 'deny' | 'continue' | 'reject' | string;
  matched?: string | null;
  // For reject the server may include the status / message.
  status?: number;
  message?: string;
}

interface TraceResponse {
  resolved: TraceResolved;
  admission: TraceDecision;
}

interface Props {
  onSessionExpired?: () => void;
}

export default function TracePanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const colors = useColors();

  const [method, setMethod] = useState<Method>('GET');
  const [path, setPath] = useState('/');
  const [query, setQuery] = useState('');
  const [sourceIp, setSourceIp] = useState('');
  const [authenticated, setAuthenticated] = useState(false);
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<TraceResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async () => {
    setRunning(true);
    setError(null);
    setResult(null);
    const body = buildTraceBody({ method, path, query, sourceIp, authenticated });
    try {
      const res = await adminFetch('/api/admin/config/trace', 'POST', body);
      if (!res.ok) {
        if (res.status === 401) onSessionExpired?.();
        await throwApiError(res, 'Trace');
      }
      const data = (await res.json()) as TraceResponse;
      setResult(data);
    } catch (e) {
      setError(normalizeUiError(e, 'unknown error'));
    } finally {
      setRunning(false);
    }
  };

  return (
    <div
      style={{
        ...contentColumn(CONTENT_FORM),
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      {/* Request form — TabHeader already titles the page "Trace". */}
      <div style={cardStyle}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
          <FormField label="Method" helpText="HTTP verb to trace. No real traffic hits the backends — this is a dry-run against the live chain.">
            <Radio.Group
              value={method}
              onChange={(e) => setMethod(e.target.value)}
              optionType="button"
              size="middle"
            >
              {METHODS.map((m) => (
                <Radio.Button key={m} value={m}>
                  {m}
                </Radio.Button>
              ))}
            </Radio.Group>
          </FormField>

          <FormField
            label="Path"
            yamlPath="(e.g. /my-bucket/path/to/object)"
            helpText="Full request path. Leading / required. Bucket is the first segment; the remainder is the object key (or empty for bucket-level ops)."
            examples={['/releases/builds/v1.zip', '/docs/readme.md', '/api/']}
            onExampleClick={(v) => setPath(String(v))}
          >
            <Input
              value={path}
              onChange={(e) => setPath(e.target.value)}
              placeholder="/my-bucket/some/key"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
            />
          </FormField>

          <FormField
            label="Query string"
            helpText="Optional. LIST operations pass `prefix=` here."
            examples={['prefix=builds/', 'list-type=2']}
            onExampleClick={(v) => setQuery(String(v))}
          >
            <Input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="(empty)"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
            />
          </FormField>

          <FormField
            label="Source IP"
            helpText="Leave empty to simulate a request with no source-IP info (source_ip predicates fail closed)."
            examples={['203.0.113.5', '198.51.100.42', '2001:db8::1']}
            onExampleClick={(v) => setSourceIp(String(v))}
          >
            <Input
              value={sourceIp}
              onChange={(e) => setSourceIp(e.target.value)}
              placeholder="(none)"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
            />
          </FormField>

          <FormField
            label="Authenticated"
            helpText="Whether the synthetic request carries SigV4 credentials."
          >
            <Space>
              <Switch checked={authenticated} onChange={setAuthenticated} />
              <Text type="secondary" style={{ fontSize: 12 }}>
                {authenticated
                  ? 'Signed (authenticated user).'
                  : 'Anonymous (no credentials).'}
              </Text>
            </Space>
          </FormField>

          <Button
            type="primary"
            icon={<ExperimentOutlined />}
            loading={running}
            onClick={run}
            style={{ marginTop: 8, borderRadius: 8, fontWeight: 600 }}
            block
            size="large"
          >
            Run trace
          </Button>
        </div>
      </div>

      {/* Error */}
      {error && (
        <Alert type="error" showIcon message="Trace failed" description={error} />
      )}

      {/* Result */}
      {result && (
        <div style={cardStyle}>
          <SectionHeader
            icon={
              decisionTone(result.admission.decision) === 'allow' ? (
                <CheckCircleOutlined />
              ) : (
                <CloseCircleOutlined />
              )
            }
            title="Decision"
            description="How the admission chain evaluated your request."
          />
          <DecisionSummary result={result} />
          <div style={{ marginTop: 16, borderTop: `1px solid ${colors.BORDER}`, paddingTop: 14 }}>
            <Text style={subHeaderStyle(colors.TEXT_MUTED)}>Reason path</Text>
            <ReasonPath result={result} />
          </div>
          <div style={{ marginTop: 16, borderTop: `1px solid ${colors.BORDER}`, paddingTop: 14 }}>
            <Text style={subHeaderStyle(colors.TEXT_MUTED)}>Resolved request</Text>
            <ResolvedRequest resolved={result.resolved} />
          </div>
          <div style={{ marginTop: 16, display: 'flex', gap: 8 }}>
            <Button
              size="small"
              onClick={() => {
                const text = JSON.stringify(result, null, 2);
                navigator.clipboard?.writeText?.(text).then(
                  () => message.success('Result copied as JSON'),
                  () => message.error('Copy failed')
                );
              }}
            >
              Copy as JSON
            </Button>
            <Button size="small" onClick={() => setResult(null)}>
              Clear
            </Button>
          </div>
        </div>
      )}

      {/* Help card */}
      {!result && !error && !running && (
        <Alert
          type="info"
          showIcon
          message="How to read the output"
          description={
            <Paragraph type="secondary" style={{ fontSize: 13, marginBottom: 0 }}>
              The trace evaluates the SAME admission chain live
              traffic hits. The Decision is the terminal action for
              this request. <b>Reason path</b> walks the evaluator
              — which block matched, which predicate, what action
              fired. <b>Resolved request</b> shows how the parser
              decomposed your path into bucket + key (helpful for
              debugging path-glob patterns).
            </Paragraph>
          }
        />
      )}
    </div>
  );
}

// ─── Helpers ─────────────────────────────────────────

/** Shared uppercase eyebrow style for the result sub-section headers. */
function subHeaderStyle(color: string): React.CSSProperties {
  return {
    fontSize: 10,
    fontWeight: 700,
    letterSpacing: 0.5,
    textTransform: 'uppercase',
    color,
    fontFamily: 'var(--font-ui)',
    display: 'block',
    marginBottom: 8,
  };
}

function decisionTone(decision: string): 'allow' | 'deny' | 'continue' {
  if (decision === 'allow-anonymous') return 'allow';
  if (decision === 'deny' || decision === 'reject') return 'deny';
  return 'continue';
}

function decisionTag(decision: string, action?: TraceDecision) {
  const tone = decisionTone(decision);
  const colour = tone === 'allow' ? 'green' : tone === 'deny' ? 'red' : 'blue';
  const label =
    decision === 'reject' && action?.status
      ? `reject ${action.status}`
      : decision;
  return (
    <Tag color={colour} style={{ fontSize: 13, padding: '2px 10px', fontWeight: 600 }}>
      {label}
    </Tag>
  );
}

function DecisionSummary({ result }: { result: TraceResponse }) {
  const colors = useColors();
  const { admission } = result;
  return (
    <div
      style={{
        marginTop: 16,
        padding: 16,
        background: colors.BG_ELEVATED,
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 10,
        display: 'flex',
        alignItems: 'center',
        gap: 14,
        flexWrap: 'wrap',
      }}
    >
      {decisionTag(admission.decision, admission)}
      {admission.matched ? (
        <>
          <Text type="secondary" style={{ fontSize: 13 }}>
            by block
          </Text>
          <Text code style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>
            {admission.matched}
          </Text>
        </>
      ) : (
        <Text type="secondary" style={{ fontSize: 13 }}>
          no block matched — chain fell through to the default
        </Text>
      )}
      {admission.decision === 'reject' && admission.message && (
        <div
          style={{
            width: '100%',
            marginTop: 6,
            fontSize: 12,
            color: colors.TEXT_SECONDARY,
            fontStyle: 'italic',
          }}
        >
          Body: {admission.message}
        </div>
      )}
    </div>
  );
}

function ReasonPath({ result }: { result: TraceResponse }) {
  const colors = useColors();
  const { resolved, admission } = result;
  const lines: string[] = [];
  // Line 1: the request, echoed back.
  const authLabel = resolved.authenticated ? 'authenticated' : 'anonymous';
  lines.push(
    `${resolved.method} ${pathOf(resolved)} from ${resolved.authenticated ? '(signed request)' : 'anonymous'}`
  );
  // Line 2: which block (if any) matched.
  if (admission.matched) {
    lines.push(`  → admission: ${admission.matched} matched`);
  } else {
    lines.push(`  → admission: no operator block matched`);
  }
  // Line 3: terminal action.
  const actionLine =
    admission.decision === 'allow-anonymous'
      ? 'action: allow-anonymous (skips SigV4 check)'
      : admission.decision === 'deny'
        ? 'action: deny (403 Forbidden, S3-style)'
        : admission.decision === 'reject'
          ? `action: reject ${admission.status ?? ''} ${admission.message ?? ''}`.trim()
          : admission.decision === 'continue'
            ? 'action: continue (fall through to next layer — SigV4 auth)'
            : `action: ${admission.decision}`;
  lines.push(`  → ${actionLine}`);

  return (
    <pre
      style={{
        margin: 0,
        padding: 14,
        background: colors.BG_ELEVATED,
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 8,
        fontFamily: 'var(--font-mono)',
        fontSize: 12,
        lineHeight: 1.7,
        color: colors.TEXT_PRIMARY,
        overflowX: 'auto',
      }}
    >
      {lines.join('\n')}
      {'\n'}
      <span style={{ color: colors.TEXT_MUTED, fontStyle: 'italic' }}>
        {/* footer — short hint for `continue` */}
        {admission.decision === 'continue'
          ? `\n(auth=${authLabel}) continue means the request is handed off to SigV4 middleware; whether it's allowed depends on IAM policies attached to the caller.`
          : ''}
      </span>
    </pre>
  );
}

function pathOf(r: TraceResolved): string {
  const base = `/${r.bucket}`;
  if (r.key) return `${base}/${r.key}`;
  if (r.list_prefix) return `${base}/ (LIST prefix=${r.list_prefix})`;
  return base;
}

function ResolvedRequest({ resolved }: { resolved: TraceResolved }) {
  const colors = useColors();
  const entries: Array<[string, string | null]> = [
    ['method', resolved.method],
    ['bucket', resolved.bucket],
    ['key', resolved.key],
    ['list_prefix', resolved.list_prefix],
    ['authenticated', String(resolved.authenticated)],
  ];
  return (
    <div
      style={{
        display: 'grid',
        gridTemplateColumns: 'auto 1fr',
        gap: '6px 14px',
        fontFamily: 'var(--font-mono)',
        fontSize: 12,
      }}
    >
      {entries.map(([k, v]) => (
        <div key={k} style={{ display: 'contents' }}>
          <span style={{ color: colors.TEXT_MUTED }}>{k}</span>
          <span style={{ color: v == null ? colors.TEXT_MUTED : colors.TEXT_PRIMARY }}>
            {v == null ? '(none)' : v}
          </span>
        </div>
      ))}
    </div>
  );
}
