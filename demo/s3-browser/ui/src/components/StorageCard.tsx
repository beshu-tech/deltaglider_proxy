import { useState } from 'react';
import { Button } from 'antd';
import { CheckCircleOutlined, ClockCircleOutlined, PauseCircleOutlined, WarningOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import { useColors } from '../ThemeContext';
import { cssVar, tint } from '../themeVars';
import { storageCardCopy, type StorageTone } from '../storageStatusView';
import { isAdminStatus } from '../adminApi';
import { useStorageStatus } from '../queries/storageStatus';
import { qk } from '../queries/keys';
import { StorageLimitEditor } from './advancedPanels';
import SectionHeader from './SectionHeader';
import { TYPE, SPACE, useCardStyles } from './shared-styles';

const ICON: Record<StorageTone, React.ReactNode> = {
  ACCENT_GREEN: <CheckCircleOutlined />,
  ACCENT_BLUE: <ClockCircleOutlined />,
  ACCENT_AMBER: <WarningOutlined />,
  ACCENT_RED: <PauseCircleOutlined />,
};

/** A sentence with its disk paths in the code face, so a long path reads as one thing. */
function Line({ text, paths }: { text: string; paths: string[] }) {
  const path = paths.find((p) => text.includes(p));
  if (!path) return <div>{text}</div>;
  const [before, after] = text.split(path);
  return (
    <div>
      {before}
      <code style={{ fontFamily: 'var(--font-mono)', fontSize: TYPE.md, overflowWrap: 'anywhere' }}>{path}</code>
      {after}
    </div>
  );
}

/**
 * The first card of the System page: is storage fine, and if not, what is
 * wrong and what to do. One sentence when healthy, and the one setting that
 * belongs to it (the largest file) behind "Change the limit". Renders nothing
 * until the first answer, and for a session that is not an admin.
 */
export default function StorageCard({ onSessionExpired }: { onSessionExpired?: () => void }) {
  const { data } = useStorageStatus();
  const { cardStyle } = useCardStyles();
  const { TEXT_PRIMARY, TEXT_MUTED, BORDER } = useColors();
  const [editing, setEditing] = useState(false);
  const queryClient = useQueryClient();
  if (!data || !isAdminStatus(data)) return null;
  const copy = storageCardCopy(data);
  const edge = copy.tone === 'ACCENT_AMBER' || copy.tone === 'ACCENT_RED' ? tint(cssVar(copy.tone), 55) : undefined;
  return (
    <div style={{ ...cardStyle, marginBottom: 0, ...(edge ? { borderColor: edge } : {}) }} data-testid="storage-card">
      <SectionHeader icon={ICON[copy.tone]} title={copy.title} tone={copy.tone}>
        <div
          style={{
            display: 'flex',
            flexDirection: 'column',
            gap: SPACE.xxs,
            marginTop: SPACE.xxs,
            fontSize: TYPE.base,
            color: TEXT_PRIMARY,
            lineHeight: 1.5,
          }}
        >
          {copy.lines.map((line) => (
            <Line key={line} text={line} paths={copy.paths} />
          ))}
          {copy.hint && <div style={{ color: TEXT_MUTED }}>{copy.hint}</div>}
          {copy.changeLimit && !editing && (
            <div>
              <Button type="link" size="small" onClick={() => setEditing(true)} style={{ padding: 0 }}>
                Change the limit
              </Button>
            </div>
          )}
        </div>
        {copy.changeLimit && editing && (
          <div style={{ marginTop: SPACE.md, paddingTop: SPACE.md, borderTop: `1px solid ${BORDER}` }}>
            <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', marginBottom: SPACE.sm }}>
              <span style={{ fontSize: TYPE.base, fontWeight: 600, color: TEXT_PRIMARY }}>Largest file</span>
              <Button type="link" size="small" onClick={() => setEditing(false)} style={{ padding: 0 }}>
                Close
              </Button>
            </div>
            <StorageLimitEditor
              onSessionExpired={onSessionExpired}
              onApplied={() => {
                // The card names the limit: read it again now, not at the next poll.
                void queryClient.invalidateQueries({ queryKey: qk.storageStatus() });
                setEditing(false);
              }}
            />
          </div>
        )}
      </SectionHeader>
    </div>
  );
}
