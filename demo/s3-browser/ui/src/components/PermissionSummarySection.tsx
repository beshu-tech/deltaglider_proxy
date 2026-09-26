import { Typography, Divider, Tag } from 'antd';
import type { IamUser, IamGroup } from '../adminApi';
import { rowsToPermissions, type PermissionRow } from './permissionRows';
import { useColors } from '../ThemeContext';
import PillButton from './PillButton';

const { Text } = Typography;

interface PermissionSummarySectionProps {
  user: IamUser;
  permissions: PermissionRow[];
  userGroups: IamGroup[];
  onNavigateToGroup?: (groupId: number) => void;
}

/**
 * "Groups & Inherited Access" + "Effective Permissions" read-only summary shown
 * under the editable permission rows when editing an existing user. Extracted
 * from UserForm so the form file stays focused on the create/edit pipeline.
 */
export default function PermissionSummarySection({ user, permissions, userGroups, onNavigateToGroup }: PermissionSummarySectionProps) {
  const colors = useColors();

  const memberGroups = userGroups.filter(g => g.member_ids.includes(user.id));
  const inheritedPerms = memberGroups.flatMap(g => g.permissions.map(p => ({ ...p, _group: g.name, _groupId: g.id })));
  const directPerms = rowsToPermissions(permissions);
  const allPerms = [...directPerms, ...inheritedPerms.map(p => ({ id: p.id, effect: p.effect, actions: p.actions, resources: p.resources, conditions: p.conditions }))];

  return (
    <div style={{ marginBottom: 24 }}>
      <Divider style={{ margin: '20px 0 12px' }}>Groups &amp; Inherited Access</Divider>

      {/* Group memberships — clickable */}
      {memberGroups.length > 0 ? (
        <div style={{ marginBottom: 16 }}>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
            {memberGroups.map(g => (
              onNavigateToGroup ? (
                <PillButton key={g.id} onClick={() => onNavigateToGroup(g.id)} title={`Open group ${g.name}`}>
                  {g.name}
                  <span style={{ marginLeft: 4, fontSize: 10 }}>
                    {g.permissions.length} rule{g.permissions.length !== 1 ? 's' : ''} · {g.member_ids.length} member{g.member_ids.length !== 1 ? 's' : ''}
                  </span>
                </PillButton>
              ) : (
                <Tag key={g.id} color="blue" style={{ borderRadius: 8, fontSize: 12, padding: '3px 12px', margin: 0 }}>
                  {g.name}
                  <span style={{ marginLeft: 4, fontSize: 10 }}>
                    {g.permissions.length} rule{g.permissions.length !== 1 ? 's' : ''} · {g.member_ids.length} member{g.member_ids.length !== 1 ? 's' : ''}
                  </span>
                </Tag>
              )
            ))}
          </div>
        </div>
      ) : (
        <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: 12 }}>
          Not a member of any group. Permissions come only from the rules above.
        </Text>
      )}

      {/* Inherited permissions — per group */}
      {memberGroups.map(g => {
        if (g.permissions.length === 0) return null;
        return (
          <div key={g.id} style={{ marginBottom: 12 }}>
            <button
              type="button"
              disabled={!onNavigateToGroup}
              onClick={() => onNavigateToGroup?.(g.id)}
              style={{
                display: 'flex', alignItems: 'center', gap: 6, marginBottom: 6, padding: 0,
                background: 'none', border: 'none', font: 'inherit',
                cursor: onNavigateToGroup ? 'pointer' : 'default',
              }}
            >
              <Text style={{ fontSize: 11, fontWeight: 700, color: colors.ACCENT_BLUE, textTransform: 'uppercase', letterSpacing: 0.5 }}>
                From {g.name}
              </Text>
              <span aria-hidden="true" style={{ fontSize: 10, color: colors.TEXT_MUTED }}>↗</span>
            </button>
            {g.permissions.map((perm, i) => {
              const isDeny = perm.effect === 'Deny';
              return (
                <div key={i} style={{
                  border: `1px solid ${isDeny ? `${colors.ACCENT_RED}20` : colors.BORDER}`,
                  borderLeft: isDeny ? `3px solid ${colors.ACCENT_RED}60` : `3px solid ${colors.ACCENT_BLUE}40`,
                  borderRadius: 8, padding: '8px 12px', marginBottom: 4,
                  background: isDeny ? `${colors.ACCENT_RED}05` : colors.BG_BASE,
                }}>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                    <Tag color={isDeny ? 'red' : 'green'} style={{ fontSize: 10, borderRadius: 4, margin: 0 }}>{perm.effect}</Tag>
                    <Text style={{ fontSize: 12 }}>
                      <strong>{perm.actions.join(', ')}</strong> on {perm.resources.join(', ')}
                    </Text>
                  </div>
                </div>
              );
            })}
          </div>
        );
      })}

      {/* Effective permissions summary — all merged */}
      {allPerms.length > 0 && (
        <div style={{
          marginTop: 16, padding: 12, borderRadius: 8,
          background: `${colors.ACCENT_BLUE}08`, border: `1px solid ${colors.ACCENT_BLUE}20`,
        }}>
          <Text style={{
            fontSize: 11, fontWeight: 700, textTransform: 'uppercase', letterSpacing: 0.5,
            color: colors.ACCENT_BLUE, display: 'block', marginBottom: 8,
          }}>
            Effective Permissions ({directPerms.length} direct + {inheritedPerms.length} inherited)
          </Text>
          {allPerms.map((perm, i) => {
            const isDeny = perm.effect === 'Deny';
            const source = i < directPerms.length ? 'direct' : inheritedPerms[i - directPerms.length]?._group;
            return (
              <div key={i} style={{
                display: 'flex', alignItems: 'center', gap: 8, padding: '4px 0',
                borderBottom: i < allPerms.length - 1 ? `1px solid ${colors.BORDER}` : undefined,
              }}>
                <Tag color={isDeny ? 'red' : 'green'} style={{ fontSize: 10, borderRadius: 4, margin: 0, flexShrink: 0 }}>
                  {perm.effect}
                </Tag>
                <Text style={{ fontSize: 12, flex: 1 }}>
                  {perm.actions.join(', ')} on {perm.resources.join(', ')}
                </Text>
                <Text style={{ fontSize: 10, color: colors.TEXT_MUTED, flexShrink: 0 }}>
                  {source}
                </Text>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
