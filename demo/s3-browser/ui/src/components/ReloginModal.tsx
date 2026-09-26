import { useEffect, useRef, useState } from 'react';
import { Alert, Input, Modal, Typography } from 'antd';
import { adminLogin, loginAs, whoami } from '../adminApi';
import { normalizeUiError } from '../errorHandling';
import { registerReloginHandler } from '../sessionRelogin';

type Mode = 'bootstrap' | 'iam' | 'open';

/**
 * The "sign in again" prompt for an admin session that expired mid-edit
 * (see sessionRelogin.ts). The page, and every unsaved edit on it, stays
 * where it is; the request that got the 401 runs again after the sign-in.
 */
export default function ReloginModal() {
  const [open, setOpen] = useState(false);
  const [mode, setMode] = useState<Mode>('bootstrap');
  const [password, setPassword] = useState('');
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const resolveRef = useRef<((ok: boolean) => void) | null>(null);

  useEffect(() => {
    const unregister = registerReloginHandler(
      () =>
        new Promise<boolean>((resolve) => {
          resolveRef.current = resolve;
          setError('');
          setPassword('');
          setSecretKey('');
          setOpen(true);
          void whoami().then((info) => setMode(info.mode));
        }),
    );
    return () => {
      unregister();
      // Unmounted mid-prompt (the admin left the page): answer, never hang.
      resolveRef.current?.(false);
      resolveRef.current = null;
    };
  }, []);

  const finish = (ok: boolean) => {
    setOpen(false);
    resolveRef.current?.(ok);
    resolveRef.current = null;
  };

  const signIn = async () => {
    setBusy(true);
    setError('');
    try {
      const result = mode === 'iam' ? await loginAs(accessKey.trim(), secretKey) : await adminLogin(password);
      if (result.ok) finish(true);
      else setError(result.error ?? 'Sign-in failed');
    } catch (e) {
      setError(normalizeUiError(e, 'Sign-in failed'));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="Your session expired"
      okText="Sign in"
      cancelText="Cancel"
      onOk={() => void signIn()}
      onCancel={() => finish(false)}
      confirmLoading={busy}
      mask={{ closable: false }}
      destroyOnHidden
    >
      <Typography.Paragraph type="secondary" style={{ fontSize: 13 }}>
        Sign in again to continue. Your unsaved changes stay on the page, and the step you started runs again.
      </Typography.Paragraph>
      <form onSubmit={(e) => { e.preventDefault(); void signIn(); }}>
        {mode === 'iam' ? (
          <>
            <label htmlFor="relogin-ak" style={{ display: 'block', fontSize: 12, marginBottom: 4 }}>Access key ID</label>
            <Input id="relogin-ak" value={accessKey} onChange={(e) => setAccessKey(e.target.value)} autoComplete="username" style={{ marginBottom: 12 }} />
            <label htmlFor="relogin-sk" style={{ display: 'block', fontSize: 12, marginBottom: 4 }}>Secret access key</label>
            <Input.Password id="relogin-sk" value={secretKey} onChange={(e) => setSecretKey(e.target.value)} autoComplete="current-password" />
          </>
        ) : (
          <>
            <label htmlFor="relogin-pw" style={{ display: 'block', fontSize: 12, marginBottom: 4 }}>Admin password</label>
            <Input.Password id="relogin-pw" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" autoFocus />
          </>
        )}
        {/* Enter submits the form. */}
        <button type="submit" hidden aria-hidden="true" tabIndex={-1} />
      </form>
      {error && <Alert type="error" showIcon title={error} style={{ marginTop: 12 }} />}
    </Modal>
  );
}
