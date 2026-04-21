/**
 * IamSourceBanner — "where does this data live?" banner for the
 * IAM panels (Users, Groups, External authentication).
 *
 * ## Why
 *
 * The YAML config's `access:` section carries only five fields:
 * `iam_mode`, `authentication`, `access_key_id`,
 * `secret_access_key`, and `bootstrap_password_hash` (the last via
 * the advanced section actually). It does NOT carry users, groups,
 * OAuth providers, or mapping rules — those live in the encrypted
 * SQLCipher `deltaglider_config.db` file, by design, in GUI mode.
 *
 * Operators routinely hit this mismatch:
 *
 *   "I added a user. Why does Copy YAML show `access: {}`?"
 *
 * Because the user is in the DB; `access:` in YAML is the
 * credential-and-mode slice, not the identity directory. This
 * banner sits at the top of the IAM panels and explains the
 * architecture in one sentence:
 *
 *   * `iam_mode: gui` (default) — DB is the source of truth.
 *     Users/groups/providers live in the encrypted DB, not YAML.
 *     `Copy YAML` on Access will NOT contain them. If you need
 *     them in YAML (for GitOps), flip to Declarative.
 *   * `iam_mode: declarative` — YAML IS the source of truth.
 *     Mutations through this panel are disabled; edit YAML +
 *     Apply instead.
 *
 * Rendering is inline (not a dismissible toast) because this is a
 * load-bearing architectural fact, not a transient notification.
 */
import { Alert } from 'antd';
import { InfoCircleOutlined, LockOutlined } from '@ant-design/icons';
import type { IamMode } from '../adminApi';

interface Props {
  iamMode: IamMode | undefined;
  /** "users", "groups", "OAuth providers", or "mapping rules" — used in the copy. */
  resource: string;
}

export default function IamSourceBanner({ iamMode, resource }: Props) {
  // Banner is deliberately consistent across modes so operators
  // always see the ownership rule; only the copy + tone change.
  if (iamMode === 'declarative') {
    return (
      <Alert
        type="warning"
        showIcon
        icon={<LockOutlined />}
        message="YAML-managed (declarative IAM mode)"
        description={
          <>
            Your proxy is running in{' '}
            <code style={{ fontFamily: 'var(--font-mono)' }}>
              access.iam_mode: declarative
            </code>
            . This means the YAML config is the source of truth for{' '}
            {resource}. Mutations through this panel return 403 — edit
            your YAML and Apply instead. The reconciler that sync-diffs
            the DB to YAML lands in Phase 3c.3; today the panel is
            read-only.
          </>
        }
        style={{ marginBottom: 16, borderRadius: 10 }}
      />
    );
  }
  return (
    <Alert
      type="info"
      showIcon
      icon={<InfoCircleOutlined />}
      message="Database-managed (GUI IAM mode)"
      description={
        <>
          In <code style={{ fontFamily: 'var(--font-mono)' }}>access.iam_mode: gui</code>
          {' '}(the default), {resource} live in the encrypted IAM
          database on disk — <b>not</b> in the YAML config. Copy YAML
          on the Access page will NOT include them. Use{' '}
          <b>IAM Backup → Export</b> (bottom-left of the sidebar) to
          snapshot the DB to JSON, or switch to{' '}
          <code style={{ fontFamily: 'var(--font-mono)' }}>declarative</code>
          {' '}mode if you want YAML to be authoritative.
        </>
      }
      style={{ marginBottom: 16, borderRadius: 10 }}
    />
  );
}
