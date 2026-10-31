/**
 * ApplyDialog — the plan → diff → apply confirmation modal.
 *
 * Renders the per-field diff returned by `validateSection` or
 * `putSection`, any warnings, and the restart-required banner. Gives
 * the operator one click to `Apply and Persist` or `Cancel` (§5.3 of
 * the admin UI revamp plan). Stolen wholesale from Nomad's apply
 * dialog — the diff itself is computed server-side so the client has
 * no ambiguity about what will actually happen.
 *
 * ## Contract
 *
 * - The parent first calls `validateSection(name, body)` and renders
 *   this dialog with the returned `response` prop.
 * - On confirm, the parent calls `putSection(name, body)` (not this
 *   component's responsibility).
 * - Dialog is stateless — reopens with fresh data each time.
 */
import { Modal, Alert, Typography, Button, Space } from 'antd';
import { LinkifiedText } from './LinkifiedText';
import { CheckCircleOutlined, WarningOutlined, SyncOutlined } from '@ant-design/icons';
import type { SectionApplyResponse, SectionName } from '../adminApi';
import { useColors } from '../ThemeContext';
import { BORDER_RADIUS } from './overlayStyles';
import { diffFields, expandDiff, formatDiffValue, type DiffLine } from '../applyDiff';
import { SPACE } from './shared-styles';
import { plainError } from '../adminApi';

const { Text } = Typography;

interface Props {
  open: boolean;
  section: SectionName;
  /** Response from a prior `validateSection` call. */
  response: SectionApplyResponse | null;
  /** Invoked when the operator confirms — parent runs the real PUT. */
  onApply: () => void;
  /** Invoked on Cancel / Esc / backdrop click. */
  onCancel: () => void;
  /** True while the parent's PUT is in flight — disables buttons. */
  loading?: boolean;
  /** Optional human-readable summary rendered above the raw diff. */
  summary?: React.ReactNode;
  /** Part of a sequence of dialogs: "Step 1 of 2 · replication rules". */
  step?: { index: number; total: number; what: string };
}

export default function ApplyDialog({
  open,
  section,
  response,
  onApply,
  onCancel,
  loading,
  summary,
  step,
}: Props) {
  const { TEXT_PRIMARY: TEXT, TEXT_MUTED, BORDER, BG_CARD } = useColors();

  if (!response) return null;

  const { ok, warnings = [], existing_warnings: existingWarnings = [], requires_restart, restart_reasons: restartReasons = [], error, diff } = response;
  const sectionDiff = (diff && diff[section]) || {};
  const diffRows = expandDiff(Object.entries(sectionDiff));
  // A diff that the server computed and found empty: nothing to apply.
  const noChanges = diff != null && diffRows.length === 0;
  // A refused body has no plan and no diff worth reading (the server sends
  // an empty one): the dialog shows the reason only (UX harvest C21).
  const refused = !ok;
  const reason = refused ? plainError(error || '', 'The proxy refused this change.') : null;

  return (
    <Modal
      open={open}
      onCancel={onCancel}
      title={
        <Space>
          <span>Review changes before applying</span>
          <Text code style={{ fontSize: 11 }}>{section}</Text>
          {step && step.total > 1 && (
            <Text type="secondary" style={{ fontSize: 12 }} data-testid="apply-dialog-step">
              Step {step.index} of {step.total} · {step.what}
            </Text>
          )}
        </Space>
      }
      width={720}
      destroyOnHidden
      footer={
        <Space>
          <Button onClick={onCancel} disabled={loading}>
            {refused ? 'Back to the form' : 'Cancel'}
          </Button>
          {!refused && (
            <Button
              data-testid="apply-dialog-confirm"
              aria-label="Apply and persist changes"
              type="primary"
              onClick={onApply}
              disabled={loading || noChanges}
              loading={loading}
              icon={<CheckCircleOutlined />}
            >
              Apply and Persist
            </Button>
          )}
        </Space>
      }
    >
      {/* Validation refused the section body: the reason, and nothing else. */}
      {reason && (
        <Alert
          type="error"
          showIcon
          icon={<WarningOutlined />}
          data-testid="apply-dialog-refused"
          title="This change cannot be applied"
          description={
            <>
              <LinkifiedText text={reason.text} />
              <div style={{ marginTop: SPACE.xxs }}>Fix it in the form, then review again. Nothing was changed.</div>
              {reason.details && (
                <details style={{ marginTop: SPACE.xs }}>
                  <summary style={{ cursor: 'pointer', fontSize: 12 }}>Details</summary>
                  <code style={{ display: 'block', marginTop: 4, fontSize: 11, overflowWrap: 'anywhere' }}>{reason.details}</code>
                </details>
              )}
            </>
          }
          style={{ marginBottom: 0 }}
        />
      )}

      {!refused && (
        <>

          {/* Warnings this change introduces (semi-blocking — operator must
              read, but Apply still available). Same contract as the field-level
              PATCH. */}
          {warnings.length > 0 && (
            <Alert
              type="warning"
              showIcon
              title={
                warnings.length === 1 ? '1 warning from this change' : `${warnings.length} warnings from this change`
              }
              description={<WarningList items={warnings} />}
              style={{ marginBottom: 12 }}
            />
          )}

          {/* Warnings the current config already had. Folded away so that they
              do not hide the warnings that matter for this change. */}
          {existingWarnings.length > 0 && (
            <details data-testid="apply-dialog-existing-warnings" style={{ marginBottom: 12, color: TEXT_MUTED }}>
              <summary style={{ cursor: 'pointer', fontSize: 13 }}>
                {existingWarnings.length === 1
                  ? '1 warning that already existed before this change'
                  : `${existingWarnings.length} warnings that already existed before this change`}
              </summary>
              <div style={{ marginTop: SPACE.xs }}>
                <WarningList items={existingWarnings} />
              </div>
            </details>
          )}

          {summary && (
            <div style={{ marginBottom: 12 }}>
              {summary}
            </div>
          )}

          {/* Diff — the star of the dialog. An empty diff disables Apply. */}
          <div
            style={{
              background: BG_CARD,
              border: `1px solid ${BORDER}`,
              borderRadius: BORDER_RADIUS.md,
              padding: 12,
              marginBottom: 12,
            }}
          >
            <div style={{ color: TEXT_MUTED, fontSize: 12, fontWeight: 600, marginBottom: SPACE.xs }}>
              Changes ({diffRows.length})
            </div>
            {diffRows.length === 0 ? (
              <Text type="secondary">No changes detected: there is nothing to apply.</Text>
            ) : (
              <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>
                {diffRows.map((line) => (
                  <DiffRow key={`${line.kind}:${line.path}`} line={line} pathColor={TEXT} />
                ))}
              </div>
            )}
            {/* The wire diff, whole, for a reader who wants every byte. */}
            {diffRows.length > 0 && (
              <details style={{ marginTop: SPACE.xs }}>
                <summary style={{ cursor: 'pointer', fontSize: 12, color: TEXT_MUTED }}>Show the raw change</summary>
                <pre
                  data-testid="apply-dialog-raw"
                  style={{ margin: `${SPACE.xs}px 0 0`, fontSize: 11, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', color: TEXT_MUTED }}
                >
                  {JSON.stringify(sectionDiff, null, 2)}
                </pre>
              </details>
            )}
          </div>

          {/* Restart-required banner — last so it's the last thing the
              operator reads before hitting Apply. */}
          {requires_restart && (
            <Alert
              type="info"
              showIcon
              icon={<SyncOutlined />}
              title="Restart required"
              description={
                <>
                  These changes take effect only after a restart of the proxy. Restart it when the window is safe.
                  {restartReasons.length > 0 && <WarningList items={restartReasons} />}
                </>
              }
              style={{ marginBottom: 0 }}
            />
          )}
        </>
      )}
    </Modal>
  );
}

function WarningList({ items }: { items: string[] }) {
  return (
    <ul style={{ marginBottom: 0, paddingLeft: SPACE.md }}>
      {items.map((w, i) => (
        <li key={i}>
          <LinkifiedText text={w} />
        </li>
      ))}
    </ul>
  );
}

/**
 * One change as a reader checks it (UX harvest D37): the field path, then
 * the old value in red and the new one in green. A whole rule that is added
 * or removed is a list of its fields, never a JSON line cut at the edge.
 */
function DiffRow({ line, pathColor }: { line: DiffLine; pathColor: string }) {
  const { ACCENT_RED, ACCENT_GREEN } = useColors();
  const side = (sign: '-' | '+', value: unknown, color: string) => {
    const fields = diffFields(value);
    if (!fields) {
      return (
        <div style={{ color, opacity: 0.9 }}>
          {sign}&nbsp;{formatDiffValue(value)}
        </div>
      );
    }
    return (
      <dl style={{ margin: 0, color, opacity: 0.9 }}>
        {fields.map((f) => (
          <div key={f.key} style={{ display: 'flex', gap: SPACE.xs }}>
            <dt style={{ flexShrink: 0 }}>
              {sign}&nbsp;{f.key}:
            </dt>
            <dd style={{ margin: 0, minWidth: 0 }}>{f.value}</dd>
          </div>
        ))}
      </dl>
    );
  };
  return (
    <div style={{ marginBottom: SPACE.sm, wordBreak: 'break-word' }}>
      <div style={{ color: pathColor, fontWeight: 600, marginBottom: 2 }}>
        {line.path}
        {line.kind !== 'changed' && (
          <Text type="secondary" style={{ fontWeight: 400, marginLeft: SPACE.xs }}>
            {line.kind === 'added' ? 'new' : 'removed'}
          </Text>
        )}
      </div>
      {line.kind !== 'added' && side('-', line.before, ACCENT_RED)}
      {line.kind !== 'removed' && side('+', line.after, ACCENT_GREEN)}
    </div>
  );
}
