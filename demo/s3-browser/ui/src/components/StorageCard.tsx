import { Button } from 'antd';
import { HddOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { cssVar, tint } from '../themeVars';
import { storageCardCopy } from '../storageStatusView';
import { isAdminStatus } from '../adminApi';
import { useStorageStatus } from '../queries/storageStatus';
import { SIZE_LIMIT_ANCHOR } from './advancedPanels';
import SectionHeader from './SectionHeader';
import { TYPE, SPACE, useCardStyles } from './shared-styles';

/** Scroll to the largest-file editor and put the cursor in its field. */
function showSizeLimitEditor() {
  const editor = document.getElementById(SIZE_LIMIT_ANCHOR);
  editor?.scrollIntoView?.({ behavior: 'smooth', block: 'start' });
  editor?.querySelector<HTMLElement>('input')?.focus();
}

/**
 * The first card of the System page: is storage fine, and if not, what is
 * wrong and what to do. One sentence when healthy. Renders nothing until the
 * first answer, and for a session that is not an admin (it has no System page).
 */
export default function StorageCard() {
  const { data } = useStorageStatus();
  const { cardStyle } = useCardStyles();
  const { TEXT_PRIMARY, TEXT_MUTED } = useColors();
  if (!data || !isAdminStatus(data)) return null;
  const copy = storageCardCopy(data);
  const edge = copy.tone === 'ACCENT_AMBER' || copy.tone === 'ACCENT_RED' ? tint(cssVar(copy.tone), 55) : undefined;
  return (
    <div style={{ ...cardStyle, marginBottom: 0, ...(edge ? { borderColor: edge } : {}) }} data-testid="storage-card">
      <SectionHeader icon={<HddOutlined />} title={copy.title} />
      <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xxs, fontSize: TYPE.base, color: TEXT_PRIMARY, lineHeight: 1.5 }}>
        {copy.lines.map((line) => (
          <div key={line}>{line}</div>
        ))}
        {copy.hint && <div style={{ color: TEXT_MUTED }}>{copy.hint}</div>}
        {copy.changeLimit && (
          <div>
            <Button type="link" size="small" onClick={showSizeLimitEditor} style={{ padding: 0 }}>
              Change the limit
            </Button>
          </div>
        )}
      </div>
    </div>
  );
}
