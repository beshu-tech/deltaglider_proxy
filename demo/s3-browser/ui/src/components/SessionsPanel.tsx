/**
 * SessionsPanel — live admin sessions and force-logout (revoke).
 *
 * Closes the security hole where a stolen admin cookie could only be killed by
 * restarting the proxy: an admin can see every live session and sign one out
 * (DELETE /sessions/:id), every other session of a principal, or every
 * session of an IAM key on every instance (revoke-user). Sessions are
 * in-memory, so a proxy restart still clears everything.
 *
 * UX harvest D55: one group per principal (not one row per sign-in), a
 * readable session line (age, address, short id), and an inline "Sign out".
 * D71: sign-out is a neutral action (teal), as in the account menu.
 * D11: with no sync bucket there are no other instances, so an identity with
 * no session here gets "Close", not a remote sign-out.
 */
import { useMemo, useState, type CSSProperties } from 'react';
import { Modal, Typography, Button, Tag, message, Input } from 'antd';
import { ReloadOutlined, LogoutOutlined, TeamOutlined } from '@ant-design/icons';
import { confirmDialog } from '../confirmDialog';
import type { SessionSummary } from '../adminApi';
import { useRevokeSession, useRevokeUserSessions, useSessions } from '../queries/sessions';
import { useAdminConfig } from '../queries/config';
import { contentColumn, CONTENT_WIDE, SPACE, CARD_GAP, useCardStyles } from './shared-styles';
import { plainError } from '../plainError';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { formatDuration } from '../utils';
import { useColors } from '../ThemeContext';
import { EmptyState, QueryState } from './StatePlaceholders';
import { groupSessions, shortSessionId, type SessionGroup } from './sessionGroups';

const { Text } = Typography;

/** A group with more sessions than this shows the newest ones and a "Show all" toggle. */
const COLLAPSED_ROWS = 3;

export default function SessionsPanel({ onSessionExpired }: { onSessionExpired?: () => void }) {
  const sessions = useSessions();
  const config = useAdminConfig();
  const revokeSessionMutation = useRevokeSession();
  const revokeUserMutation = useRevokeUserSessions();
  const rows = sessions.data ?? [];
  const [busy, setBusy] = useState<string | null>(null);
  const [revokeKey, setRevokeKey] = useState('');
  const { cardStyle } = useCardStyles();
  const groups = useMemo(() => groupSessions(sessions.data ?? []), [sessions.data]);

  useSessionExpiredOn(sessions.error, onSessionExpired);

  // Other instances exist only behind a sync bucket. While the config is
  // unknown (loading, or its load failed) the remote sign-out stays offered.
  const multiInstance = config.data ? !!config.data.config_sync_bucket : true;

  const revokeIds = async (ids: string[], busyKey: string) => {
    let ended = 0;
    try {
      setBusy(busyKey);
      for (const id of ids) {
        await revokeSessionMutation.mutateAsync(id);
        ended += 1;
      }
      message.success(ended === 1 ? 'The session is signed out.' : `${ended} sessions are signed out.`);
    } catch (e) {
      const prefix = ended > 0 ? `${ended} of ${ids.length} sessions are signed out. ` : '';
      message.error(prefix + plainError(e, 'The proxy could not sign out the session.').text);
    } finally {
      setBusy(null);
    }
  };

  const signOutOne = async (s: SessionSummary, group: SessionGroup) => {
    const ok = await confirmDialog({
      title: 'Sign out this session?',
      content: `${group.label}, signed in ${formatDuration(s.age_secs)} ago${s.ip ? ` from ${s.ip}` : ''}. The browser of this session returns to the sign-in screen at its next request.`,
      okText: 'Sign out',
    });
    if (ok) await revokeIds([s.id], s.id);
  };

  const signOutOthers = async (group: SessionGroup) => {
    const n = group.others.length;
    const ok = await confirmDialog({
      title: group.hasCurrent ? `Sign out your ${n} other session${n === 1 ? '' : 's'}?` : `Sign out ${n === 1 ? 'the session' : `all ${n} sessions`} of ${group.label}?`,
      content: group.hasCurrent
        ? 'Your current session stays signed in. The other browsers return to the sign-in screen at their next request.'
        : 'Each browser returns to the sign-in screen at its next request. This ends the sessions on this instance only.',
      okText: 'Sign out',
    });
    if (ok) await revokeIds(group.others.map((s) => s.id), `group:${group.key}`);
  };

  const revokeUser = async () => {
    const key = revokeKey.trim();
    if (!key) return;
    // Say what the click does: the sessions it ends here, or that none here
    // uses the identity (the sign-out still reaches the other instances,
    // where this list cannot look — when there are other instances).
    const here = rows.filter((r) => r.identity === key).length;
    let confirmed: boolean;
    if (here > 0) {
      confirmed = await confirmDialog({
        title: `Sign out ${here === 1 ? 'the session' : `all ${here} sessions`} of "${key}"?`,
        content: 'This ends the sessions on every instance. If this is your own identity, you are signed out too. Use it after you rotate a compromised key.',
        okText: 'Sign out all sessions',
      });
    } else if (!multiInstance) {
      Modal.info({
        title: `No live session uses "${key}"`,
        content: 'Check the access key ID. This proxy runs as one instance (no sync bucket), so there is nothing to sign out.',
        okText: 'Close',
      });
      return;
    } else {
      confirmed = await new Promise<boolean>((resolve) => {
        Modal.confirm({
          title: `No live session on this instance uses "${key}"`,
          content: 'Check the access key ID. A session of this identity can still be live on another instance: the sign-out also ends those.',
          okText: 'Sign out on other instances',
          okButtonProps: { type: 'default' },
          cancelText: 'Close',
          onOk: () => resolve(true),
          onCancel: () => resolve(false),
        });
      });
    }
    if (!confirmed) return;
    try {
      setBusy('user');
      const res = await revokeUserMutation.mutateAsync(key);
      const local = `Revoked ${res.revoked_local} session${res.revoked_local === 1 ? '' : 's'} locally`;
      if (!res.persisted) {
        message.warning(`${local} — but the revocation could NOT be persisted; sessions on OTHER instances stay valid. Retry, or restart the peers.`);
      } else {
        message.success(`${local}; peers converge within ~${Math.ceil((res.propagation_bound_secs ?? 300) / 60)} min`);
      }
      setRevokeKey('');
    } catch (e) {
      message.error(plainError(e, 'The proxy could not sign out the sessions of this identity.').text);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div style={{ ...contentColumn(CONTENT_WIDE), padding: `${SPACE.md}px`, display: 'flex', flexDirection: 'column', gap: CARD_GAP }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.sm, flexWrap: 'wrap' }}>
        <Button icon={<ReloadOutlined />} onClick={() => void sessions.refetch()} loading={sessions.isFetching}>
          Refresh
        </Button>
        {sessions.data && (
          <Text type="secondary">
            {rows.length} live session{rows.length === 1 ? '' : 's'}
            {groups.length > 0 && ` · ${groups.length} signed-in principal${groups.length === 1 ? '' : 's'}`}
          </Text>
        )}
      </div>
      <QueryState
        query={sessions}
        what="the sessions"
        isEmpty={(d) => d.length === 0}
        empty={<EmptyState icon={<TeamOutlined />} title="Nobody is signed in" hint="A session starts when someone signs in to the admin settings or the file browser." />}
      >
        {(data) => (
          <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.md }}>
            {groupSessions(data).map((g) => (
              <SessionGroupCard
                key={g.key}
                group={g}
                cardStyle={cardStyle}
                busy={busy}
                onSignOut={(s) => void signOutOne(s, g)}
                onSignOutOthers={() => void signOutOthers(g)}
              />
            ))}
          </div>
        )}
      </QueryState>
      <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xs, maxWidth: 480, width: '100%' }}>
        <label htmlFor="sessions-revoke-identity" style={{ fontSize: 13.5, fontWeight: 600 }}>
          Sign out every session of an identity
        </label>
        <div style={{ display: 'flex', gap: SPACE.xs }}>
          <Input
            id="sessions-revoke-identity"
            aria-describedby="sessions-revoke-identity-help"
            placeholder="Access key or provider:user"
            value={revokeKey}
            onChange={(e) => setRevokeKey(e.target.value)}
            onPressEnter={() => void revokeUser()}
          />
          <Button loading={busy === 'user'} disabled={!revokeKey.trim()} onClick={() => void revokeUser()}>
            Revoke key
          </Button>
        </div>
        <Text id="sessions-revoke-identity-help" type="secondary" style={{ fontSize: 12.5 }}>
          An IAM user&apos;s access key ID, or provider:user-id for a user who signs in with an external provider.
          {multiInstance ? ' This also ends the sessions on the other instances.' : ''}
        </Text>
      </div>
    </div>
  );
}

function SessionGroupCard({
  group,
  cardStyle,
  busy,
  onSignOut,
  onSignOutOthers,
}: {
  group: SessionGroup;
  cardStyle: CSSProperties;
  busy: string | null;
  onSignOut: (s: SessionSummary) => void;
  onSignOutOthers: () => void;
}) {
  const { TEXT_MUTED, BORDER } = useColors();
  const [expanded, setExpanded] = useState(false);
  const shown = expanded ? group.sessions : group.sessions.slice(0, COLLAPSED_ROWS);
  const hidden = group.sessions.length - shown.length;
  const n = group.others.length;
  return (
    <section aria-label={`Sessions of ${group.label}`} style={{ ...cardStyle, padding: SPACE.md }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.sm, flexWrap: 'wrap' }}>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ fontWeight: 600, fontSize: 14, overflowWrap: 'anywhere' }}>{group.label}</div>
          <div style={{ fontSize: 12.5, color: TEXT_MUTED }}>
            {group.method} · {group.sessions.length} session{group.sessions.length === 1 ? '' : 's'}
          </div>
        </div>
        {n > 0 && group.sessions.length > 1 && (
          <Button size="small" icon={<LogoutOutlined />} loading={busy === `group:${group.key}`} onClick={onSignOutOthers}>
            {group.hasCurrent ? `Sign out ${n} other${n === 1 ? '' : 's'}` : `Sign out all ${n}`}
          </Button>
        )}
      </div>
      <ul style={{ listStyle: 'none', margin: `${SPACE.sm}px 0 0`, padding: 0 }}>
        {shown.map((s) => (
          <li
            key={s.id}
            style={{ display: 'flex', alignItems: 'center', gap: SPACE.sm, padding: `${SPACE.xs}px 0`, borderTop: `1px solid ${BORDER}`, flexWrap: 'wrap' }}
          >
            <div style={{ flex: 1, minWidth: 0, fontSize: 13 }}>
              Signed in {formatDuration(s.age_secs)} ago{s.ip ? ` from ${s.ip}` : ''}
              <span style={{ color: TEXT_MUTED }} title={`Session ID ${s.id}`}>
                {' '}· {s.admin_gui ? 'Admin settings' : 'File browser'} · ID {shortSessionId(s.id)}
              </span>
            </div>
            {s.current ? (
              <Tag style={{ margin: 0 }} title="Use Sign out in the account menu to end your own session">
                This browser
              </Tag>
            ) : (
              <Button
                size="small"
                aria-label={`Sign out session ${s.id}`}
                loading={busy === s.id}
                onClick={() => onSignOut(s)}
              >
                Sign out
              </Button>
            )}
          </li>
        ))}
      </ul>
      {(hidden > 0 || expanded) && group.sessions.length > COLLAPSED_ROWS && (
        <Button type="link" size="small" style={{ paddingInline: 0 }} aria-expanded={expanded} onClick={() => setExpanded((v) => !v)}>
          {expanded ? 'Show fewer' : `Show ${hidden} more`}
        </Button>
      )}
    </section>
  );
}
