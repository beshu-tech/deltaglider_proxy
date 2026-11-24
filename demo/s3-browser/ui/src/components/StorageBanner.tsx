import { Button } from 'antd';
import { cssVar, noticeBand } from '../themeVars';
import { storageBannerCopy } from '../storageStatusView';
import { isAdminStatus } from '../adminApi';
import { useStorageStatus } from '../queries/storageStatus';
import { TYPE, SPACE } from './shared-styles';

/**
 * Admin pages only: a line when the disk is filling up or uploads are paused,
 * with the way to the Storage card. Nothing when healthy, while checking, or
 * for a session that is not an admin (it sees no privileged link).
 */
export default function StorageBanner({ enabled, onFix }: { enabled: boolean; onFix: () => void }) {
  const { data } = useStorageStatus(enabled);
  if (!enabled || !data || !isAdminStatus(data)) return null;
  const copy = storageBannerCopy(data);
  if (!copy) return null;
  return (
    <div
      // A pause is urgent; a filling disk is not.
      role={copy.tone === 'ACCENT_RED' ? 'alert' : 'status'}
      aria-label="Storage"
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        flexWrap: 'wrap',
        gap: SPACE.xs,
        padding: `${SPACE.xs}px ${SPACE.md}px`,
        fontSize: TYPE.md,
        ...noticeBand(copy.tone),
      }}
    >
      <span style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
        <span aria-hidden style={{ width: SPACE.xs, height: SPACE.xs, borderRadius: '50%', background: cssVar(copy.tone), flex: 'none' }} />
        {copy.text}
      </span>
      <Button type="link" size="small" onClick={onFix} style={{ padding: 0, fontWeight: 600 }}>
        Fix it →<span className="sr-only">: open Storage on the System page</span>
      </Button>
    </div>
  );
}
