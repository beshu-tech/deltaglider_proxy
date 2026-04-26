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
import { CheckCircleOutlined, WarningOutlined, SyncOutlined } from '@ant-design/icons';
import type { SectionApplyResponse, SectionName } from '../adminApi';
import { useColors } from '../ThemeContext';

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
}

export default function ApplyDialog({
  open,
  section,
  response,
  onApply,
  onCancel,
  loading,
  summary,
}: Props) {
  const { TEXT_PRIMARY: TEXT, TEXT_MUTED, BORDER, BG_CARD } = useColors();

  if (!response) return null;

  const { ok, warnings = [], requires_restart, error, diff } = response;
  const sectionDiff = (diff && diff[section]) || {};
  const diffRows = Object.entries(sectionDiff);

  return (
    <Modal
      open={open}
      onCancel={onCancel}
      title={
        <Space>
          <span>Review changes before applying</span>
          <Text code style={{ fontSize: 11 }}>{section}</Text>
        </Space>
      }
      width={720}
      destroyOnHidden
      footer={
        <Space>
          <Button onClick={onCancel} disabled={loading}>
            Cancel
          </Button>
          <Button
            type="primary"
            onClick={onApply}
            disabled={!ok || loading}
            loading={loading}
            icon={<CheckCircleOutlined />}
          >
            Apply and Persist
          </Button>
        </Space>
      }
    >
      {/* Top-level error: validation refused the section body */}
      {!ok && error && (
        <Alert
          type="error"
          showIcon
          icon={<WarningOutlined />}
          message="Validation failed"
          description={error}
          style={{ marginBottom: 12 }}
        />
      )}

      {/* Warnings (semi-blocking — operator must read, but Apply still
          available). Same contract as the field-level PATCH. */}
      {warnings.length > 0 && (
        <Alert
          type="warning"
          showIcon
          message={warnings.length === 1 ? '1 warning' : `${warnings.length} warnings`}
          description={
            <ul style={{ marginBottom: 0, paddingLeft: 20 }}>
              {warnings.map((w, i) => (
                <li key={i}>{w}</li>
              ))}
            </ul>
          }
          style={{ marginBottom: 12 }}
        />
      )}

      {summary && (
        <div style={{ marginBottom: 12 }}>
          {summary}
        </div>
      )}

      {/* Diff — the star of the dialog. Empty diff = no-op apply, which
          the UI still allows (matches field-level PATCH idempotency). */}
      <div
        style={{
          background: BG_CARD,
          border: `1px solid ${BORDER}`,
          borderRadius: 8,
          padding: 12,
          marginBottom: 12,
        }}
      >
        <div
          style={{
            color: TEXT_MUTED,
            fontSize: 11,
            fontWeight: 600,
            letterSpacing: 0.5,
            textTransform: 'uppercase',
            marginBottom: 8,
          }}
        >
          Changes ({diffRows.length})
        </div>
        {diffRows.length === 0 ? (
          <Text type="secondary">No changes detected — this apply would be a no-op.</Text>
        ) : (
          <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>
            {diffRows.map(([path, change]) => (
              <div key={path} style={{ marginBottom: 10 }}>
                <div style={{ color: TEXT, fontWeight: 600, marginBottom: 2 }}>{path}</div>
                <div style={{ color: '#d1617a', opacity: 0.9 }}>
                  -&nbsp;{formatValue(change.before)}
                </div>
                <div style={{ color: '#4bc77a', opacity: 0.9 }}>
                  +&nbsp;{formatValue(change.after)}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Restart-required banner — last so it's the last thing the
          operator reads before hitting Apply. */}
      {requires_restart && (
        <Alert
          type="info"
          showIcon
          icon={<SyncOutlined />}
          message="Restart required"
          description="One or more changed fields (listen_addr, cache_size_mb) require a server restart to take effect. The apply will persist to disk — restart the proxy when the window is safe."
          style={{ marginBottom: 0 }}
        />
      )}
    </Modal>
  );
}

/** Format any JSON-ish value for diff display. Keeps JSON shape but
 *  collapses long arrays/objects to a single-line preview so the diff
 *  stays readable. */
function formatValue(v: unknown): string {
  if (v === null || v === undefined) return 'null';
  if (typeof v === 'string') return JSON.stringify(v);
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  try {
    const s = JSON.stringify(v);
    if (s.length <= 120) return s;
    return s.slice(0, 117) + '...';
  } catch {
    return String(v);
  }
}
