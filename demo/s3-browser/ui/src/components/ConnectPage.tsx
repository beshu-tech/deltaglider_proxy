import { useState, useEffect, useCallback, useRef } from 'react';
import { Button, Input, Typography, Space, Alert, Spin, message } from 'antd';
import { WarningOutlined, CheckCircleOutlined, CopyOutlined, SunOutlined, MoonOutlined } from '@ant-design/icons';
import { testConnection, setEndpoint, setCredentials, setBucket, initFromSession, getBucket } from '../s3client';
import { adminLogin, loginAs, whoami, isConfigDbLocked, recoverDb, browserSessionConnect, openBrowserConnect } from '../adminApi';
import type { ExternalProviderInfo } from '../adminApi';
import OAuthProviderList from './OAuthProviderList';
import { detectDefaultEndpoint } from '../utils';
import { useColors, useTheme } from '../ThemeContext';
import { normalizeUiError } from '../errorHandling';

const { Text } = Typography;

/** Set on sign-out so open-access mode does not immediately auto-reconnect. */
const SESSION_USER_SIGNED_OUT = 'dg-session-user-signed-out';

function clearSignedOutFlag() {
  try {
    sessionStorage.removeItem(SESSION_USER_SIGNED_OUT);
  } catch {
    /* private mode */
  }
}

/** What kind of session the connect flow established, decided at the point it's
 *  KNOWN (the loginAs-vs-lift branch) rather than re-derived by the caller. */
export type ConnectOutcome =
  | { kind: 'admin' } // bootstrap login / IAM admin login-as → full admin GUI
  | { kind: 'filesOnly' } // IAM non-admin browser-lift → browse only
  | { kind: 'open' }; // open-access anonymous session

function finishConnect(onConnect: (outcome: ConnectOutcome) => void, outcome: ConnectOutcome) {
  clearSignedOutFlag();
  onConnect(outcome);
}

/** Tiny diagram for the locked-DB wizard: one password + random salt → two
 *  different hashes; only the original hash is the DB's key. Explains, honestly
 *  and at a glance, why the password alone can't unlock the database. */
function LockedDbExplainer({ accent, muted, text }: { accent: string; muted: string; text: string }) {
  const ok = 'var(--accent-success)';
  const warn = 'var(--accent-warning)';
  const mono = 'var(--font-mono)';
  const Pill = ({ color, children }: { color: string; children: React.ReactNode }) => (
    <span style={{ fontFamily: mono, fontSize: 11, color, border: `1px solid ${color}`, borderRadius: 6, padding: '2px 7px', whiteSpace: 'nowrap' }}>{children}</span>
  );
  const Arrow = ({ label }: { label: string }) => (
    <span style={{ display: 'inline-flex', flexDirection: 'column', alignItems: 'center', color: muted, fontSize: 10, fontFamily: 'var(--font-ui)' }}>
      <span>{label}</span>
      <span style={{ fontSize: 14, lineHeight: 1, color: muted }}>→</span>
    </span>
  );
  return (
    <div style={{ marginTop: 14, padding: 14, borderRadius: 10, background: 'color-mix(in srgb, var(--input-bg) 80%, var(--glass-bg) 20%)', border: '1px solid var(--border-subtle)' }}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
          <Pill color={accent}>your password</Pill>
          <Arrow label="+ salt A" />
          <Pill color={ok}>hash A</Pill>
          <span style={{ color: ok, fontSize: 13, fontFamily: 'var(--font-ui)' }}>🔓 unlocks the DB</span>
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
          <Pill color={accent}>your password</Pill>
          <Arrow label="+ salt B" />
          <Pill color={warn}>hash B</Pill>
          <span style={{ color: warn, fontSize: 13, fontFamily: 'var(--font-ui)' }}>✗ wrong key</span>
        </div>
      </div>
      <div style={{ marginTop: 10, fontSize: 11.5, lineHeight: 1.55, color: text, fontFamily: 'var(--font-ui)' }}>
        Same password, random salt → a new hash each time. The DB only opens with the
        <b> exact hash</b> that encrypted it (hash A) — not one freshly made from the password.
      </div>
    </div>
  );
}

interface Props {
  onConnect: (outcome: ConnectOutcome) => void;
  showError?: boolean;
}

export default function ConnectPage({ onConnect, showError }: Props) {
  const { BORDER, TEXT_MUTED, TEXT_PRIMARY, TEXT_SECONDARY, ACCENT_BLUE } = useColors();
  const { isDark, toggleTheme } = useTheme();
  const [accessKey, setAccessKey] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [adminPassword, setAdminPassword] = useState('');
  const [loading, setLoading] = useState(false);
  // Synchronous guard — setLoading is async React state, so two Enter-triggered
  // calls in the same tick (onPressEnter + button onClick) both see loading===false
  // and double-fire.  The ref is set synchronously before the first await.
  const loginInFlight = useRef(false);
  const [error, setError] = useState('');
  const [authMode, setAuthMode] = useState<'bootstrap' | 'iam' | 'open' | null>(null);
  const [externalProviders, setExternalProviders] = useState<ExternalProviderInfo[]>([]);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [detecting, setDetecting] = useState(true);
  const [openSignedOut, setOpenSignedOut] = useState(false);
  // Recovery wizard state — persist success in sessionStorage so refresh doesn't reset
  const [showRecovery, setShowRecovery] = useState(false);
  const [recoveryPassword, setRecoveryPassword] = useState('');
  const [recoveryLoading, setRecoveryLoading] = useState(false);
  const [recoveryError, setRecoveryError] = useState('');
  // Seeded from sessionStorage once on mount and intentionally NOT refetched when
  // `showRecovery` later flips true: a successful recovery survives a page refresh
  // (whoami() still reports config_db_mismatch until the operator updates the server
  // config), so the user keeps seeing the hash they need to copy. We must not clear it
  // when re-entering the wizard or that recovered hash would be lost.
  const [recoveredHash, setRecoveredHash] = useState<{ hash: string; base64: string } | null>(() => {
    try {
      const saved = sessionStorage.getItem('dg-recovered-hash');
      return saved ? JSON.parse(saved) : null;
    } catch { return null; }
  });
  const [messageApi, contextHolder] = message.useMessage();

  const runOpenModeConnect = useCallback(async (): Promise<{ ok: true } | { ok: false; error: string }> => {
    const endpoint = detectDefaultEndpoint().replace(/\/+$/, '');
    setEndpoint(endpoint);
    const result = await testConnection(endpoint, 'anonymous', 'anonymous').catch(
      () => ({ ok: false, error: '' } as const),
    );
    if (!result.ok) {
      // Surface the server's real reason, not a blanket "backend unreachable".
      // Mount-time uses the typed lock signal (isConfigDbLocked); this raw-S3
      // path only has the error string, so it sniffs it as a last resort.
      const reason = 'error' in result ? result.error : '';
      const looksLocked = /bootstrap|mismatch|recover/i.test(reason || '');
      return {
        ok: false,
        error: looksLocked
          ? `${reason} — open Settings (/_/) to recover.`
          : reason
            ? `Could not reach the S3 API: ${reason}`
            : 'Could not reach the S3 API. The server may still be starting, or its storage backend is misconfigured.',
      };
    }
    if (result.buckets && result.buckets.length > 0) {
      setBucket(result.buckets[0]);
    }
    const ob = await openBrowserConnect({ endpoint, bucket: getBucket() });
    if (!ob.ok) {
      return { ok: false, error: ob.error || 'Could not connect. Try again.' };
    }
    const restored = await initFromSession();
    if (!restored) {
      return { ok: false, error: 'Open session created but credentials could not be restored.' };
    }
    return { ok: true };
  }, []);

  // Detect auth mode on mount — auto-connect in open mode, show recovery wizard if mismatch.
  // `cancelled` guards against setState after unmount / dep change while whoami() or
  // runOpenModeConnect() are still pending (same pattern as DocsPage's Mermaid effect).
  useEffect(() => {
    let cancelled = false;
    whoami()
      .then(async (info) => {
        if (cancelled) return;
        setAuthMode(info.mode as 'bootstrap' | 'iam' | 'open');
        setExternalProviders(info.external_providers || []);
        if (isConfigDbLocked(info)) {
          setShowRecovery(true);
          setDetecting(false);
          return;
        }
        // In open access mode, auto-connect with the proxy's own endpoint (no credentials needed)
        if (info.mode === 'open') {
          let signedOut = false;
          try {
            signedOut = sessionStorage.getItem(SESSION_USER_SIGNED_OUT) === '1';
          } catch {
            /* private mode */
          }
          if (signedOut) {
            setOpenSignedOut(true);
            setDetecting(false);
            return;
          }
          const r = await runOpenModeConnect();
          if (cancelled) return;
          if (!r.ok) {
            setDetecting(false);
            setError(r.error);
            return;
          }
          finishConnect(onConnect, { kind: 'open' });
          return;
        }
        setDetecting(false);
      })
      .catch(() => {
        if (!cancelled) setDetecting(false);
      });
    return () => {
      cancelled = true;
    };
  }, [onConnect, runOpenModeConnect]);

  const handleConnect = async () => {
    if (loginInFlight.current) return;
    loginInFlight.current = true;
    setLoading(true);
    setError('');
    try {
      if (authMode === 'bootstrap') {
        // Bootstrap mode: login with password, session auto-provides S3 creds
        if (!adminPassword.trim()) {
          setError('Admin password is required');
          setLoading(false);
          return;
        }
        const adminResult = await adminLogin(adminPassword);
        if (!adminResult.ok) {
          setError(`Login failed: ${adminResult.error || 'Invalid password'}`);
          setLoading(false);
          return;
        }

        // Check for config DB lock after successful login
        const info = await whoami();
        if (isConfigDbLocked(info)) {
          setShowRecovery(true);
          setLoading(false);
          return;
        }

        const restored = await initFromSession();
        if (restored) {
          finishConnect(onConnect, { kind: 'admin' });
          return;
        }
        // Session didn't provide creds — shouldn't happen in bootstrap, but fall through
        setError('Login succeeded but no S3 credentials available. Check server config.');
        setLoading(false);
        return;
      }

      // IAM mode: connect with S3 credentials
      const cleanEndpoint = detectDefaultEndpoint().replace(/\/+$/, '');
      if (!accessKey.trim() || !secretKey.trim()) {
        setError('Access Key and Secret Key are required');
        setLoading(false);
        return;
      }
      const result = await testConnection(cleanEndpoint, accessKey, secretKey);
      if (!result.ok) {
        setError(`Connection failed: ${result.error || 'Invalid credentials'}`);
        setLoading(false);
        return;
      }

      setEndpoint(cleanEndpoint);
      if (result.buckets && result.buckets.length > 0) {
        setBucket(result.buckets[0]);
      }

      const trimmedAk = accessKey.trim();
      const trimmedSk = secretKey.trim();

      // Admin session must exist before PUT /session/s3-credentials (cookie-gated).
      // login-as succeeds only for IAM admins → admin GUI; otherwise we fall back
      // to a browse-only lift session. The outcome is KNOWN here, so pass it on
      // rather than have the caller re-derive it.
      let iamOutcome: ConnectOutcome = { kind: 'admin' };
      const loginAsRes = await loginAs(trimmedAk, trimmedSk);
      if (loginAsRes.ok) {
        setCredentials(trimmedAk, trimmedSk);
      } else {
        iamOutcome = { kind: 'filesOnly' };
        const bc = await browserSessionConnect({
          access_key_id: trimmedAk,
          secret_access_key: trimmedSk,
          endpoint: cleanEndpoint,
          bucket: getBucket(),
        });
        if (!bc.ok) {
          setError(bc.error || 'Could not connect. Try again.');
          setLoading(false);
          return;
        }
        const restored = await initFromSession();
        if (!restored) {
          setError('Session created but credentials could not be restored.');
          setLoading(false);
          return;
        }
      }

      finishConnect(onConnect, iamOutcome);
    } catch (e) {
      setError(normalizeUiError(e, "Connection failed"));
    } finally {
      loginInFlight.current = false;
      setLoading(false);
    }
  };

  const handleOpenReconnect = async () => {
    setError('');
    setLoading(true);
    try {
      clearSignedOutFlag();
      setOpenSignedOut(false);
      const r = await runOpenModeConnect();
      if (!r.ok) {
        setError(r.error);
        setOpenSignedOut(true);
        return;
      }
      finishConnect(onConnect, { kind: 'open' });
    } finally {
      setLoading(false);
    }
  };

  const handleRecover = async () => {
    if (!recoveryPassword.trim()) return;
    setRecoveryLoading(true);
    setRecoveryError('');
    try {
      const result = await recoverDb(recoveryPassword);
      if (result.success && result.correct_hash) {
        const recovered = {
          hash: result.correct_hash,
          base64: result.correct_hash_base64 || '',
        };
        setRecoveredHash(recovered);
        try { sessionStorage.setItem('dg-recovered-hash', JSON.stringify(recovered)); } catch { /* Safari private mode — fine to skip */ }
      } else {
        setRecoveryError(result.error || 'Password does not match');
      }
    } catch (e) {
      setRecoveryError(normalizeUiError(e, 'Recovery failed'));
    } finally {
      setRecoveryLoading(false);
    }
  };

  const copyToClipboard = (text: string, label: string) => {
    navigator.clipboard.writeText(text).then(() => {
      messageApi.success(`${label} copied to clipboard`);
    });
  };

  const isBootstrap = authMode === 'bootstrap';
  const isOpenSignedOutReconnect = authMode === 'open' && openSignedOut;
  const canSubmit = isBootstrap ? adminPassword.trim() : (accessKey.trim() && secretKey.trim());

  const inputStyle = {
    background: 'var(--input-bg)',
    borderColor: BORDER,
    borderRadius: 10,
    height: 44,
    fontFamily: "var(--font-mono)" as const,
    fontSize: 13,
  };
  const loginInputStyle = {
    ...inputStyle,
    background: 'color-mix(in srgb, var(--input-bg) 82%, transparent)',
    borderColor: 'color-mix(in srgb, var(--glass-border) 82%, var(--focus-ring) 18%)',
    borderRadius: 16,
    height: 52,
    fontSize: 14,
    boxShadow: 'inset 0 1px 0 rgba(255, 255, 255, 0.04)',
  };
  const recoveryInputStyle = {
    ...inputStyle,
    background: 'color-mix(in srgb, var(--input-bg) 62%, var(--login-panel-bg) 38%)',
    borderColor: 'color-mix(in srgb, var(--login-card-border) 86%, var(--focus-ring) 14%)',
    color: TEXT_PRIMARY,
    borderRadius: 12,
  };

  if (detecting) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '100vh' }}>
        <Spin size="large" />
      </div>
    );
  }

  // Recovery wizard
  if (showRecovery) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', minHeight: '100vh', padding: 24 }}>
        {contextHolder}
        <div className="dg-login-card animate-fade-in" style={{ borderRadius: 14, padding: 'clamp(28px, 4vw, 40px)', width: '100%', maxWidth: 520 }}>
          <Space direction="vertical" size="large" style={{ width: '100%' }}>
            {recoveredHash ? (
              <>
                <div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 12 }}>
                    <CheckCircleOutlined style={{ fontSize: 28, color: 'var(--accent-success)', flexShrink: 0 }} />
                    <div style={{ fontSize: 20, fontWeight: 700, color: TEXT_PRIMARY, fontFamily: "var(--font-ui)" }}>
                      Database Recovered
                    </div>
                  </div>
                  <div style={{ color: TEXT_SECONDARY, fontSize: 14, fontFamily: "var(--font-ui)", lineHeight: 1.7 }}>
                    Set this hash as your admin password (env <code>DGP_BOOTSTRAP_PASSWORD_HASH</code>
                    or the config), then restart the server to unlock. Your original config
                    database is preserved on the server as <code>deltaglider_config.db.bak</code> and
                    is not modified by recovery.
                  </div>
                </div>
                <div style={{ background: 'color-mix(in srgb, var(--input-bg) 78%, var(--glass-bg) 22%)', borderRadius: 12, padding: 16 }}>
                  <div style={{ marginBottom: 12 }}>
                    <label style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Hash</label>
                    <div style={{ display: 'flex', gap: 8, marginTop: 4 }}>
                      <Input value={recoveredHash.hash} readOnly style={{ ...recoveryInputStyle, flex: 1, fontSize: 11 }} />
                      <Button icon={<CopyOutlined />} onClick={() => copyToClipboard(recoveredHash.hash, 'Hash')} />
                    </div>
                  </div>
                  <div>
                    <label style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>Base64 (for Docker / env vars)</label>
                    <div style={{ display: 'flex', gap: 8, marginTop: 4 }}>
                      <Input value={recoveredHash.base64} readOnly style={{ ...recoveryInputStyle, flex: 1, fontSize: 11 }} />
                      <Button icon={<CopyOutlined />} onClick={() => copyToClipboard(recoveredHash.base64, 'Base64 hash')} />
                    </div>
                  </div>
                </div>
                <Alert type="info" showIcon message={
                  <span style={{ fontFamily: "var(--font-ui)", fontSize: 12 }}>
                    Set <code style={{ fontFamily: "var(--font-mono)" }}>DGP_BOOTSTRAP_PASSWORD_HASH</code> in your environment
                    or <code style={{ fontFamily: "var(--font-mono)" }}>advanced.bootstrap_password_hash</code> in your YAML config, then restart.
                  </span>
                } />
              </>
            ) : (
              <>
                <div>
                  <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 12 }}>
                    <WarningOutlined style={{ fontSize: 28, color: 'var(--accent-warning)', flexShrink: 0 }} />
                    <div style={{ fontSize: 20, fontWeight: 700, color: TEXT_PRIMARY, fontFamily: "var(--font-ui)" }}>
                      Config Database Locked
                    </div>
                  </div>
                  <div style={{ color: TEXT_SECONDARY, fontSize: 14, lineHeight: 1.7, fontFamily: "var(--font-ui)" }}>
                    The IAM database is encrypted with your admin password <b>hash</b>, and
                    the hash loaded at startup doesn’t match the one that encrypted it.
                  </div>
                  <LockedDbExplainer accent={ACCENT_BLUE} muted={TEXT_MUTED} text={TEXT_SECONDARY} />
                  <div style={{ color: TEXT_SECONDARY, fontSize: 13, marginTop: 12, lineHeight: 1.7, fontFamily: "var(--font-ui)" }}>
                    <b>Why not just the password?</b> Each hash bakes in a random salt, so the
                    same password produces a <i>different</i> hash every time — only the exact
                    original hash can unlock the DB. Paste it from your previous{' '}
                    <code style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: ACCENT_BLUE }}>DGP_BOOTSTRAP_PASSWORD_HASH</code>,
                    env vars, or the <code style={{ fontFamily: "var(--font-mono)", fontSize: 12, color: ACCENT_BLUE }}>.deltaglider_bootstrap_hash</code> file.
                  </div>
                  <div style={{ color: TEXT_MUTED, fontSize: 12, marginTop: 8, lineHeight: 1.6, fontFamily: "var(--font-ui)" }}>
                    Lost the hash? It can’t be recovered — but your data is safe. Run{' '}
                    <code style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: TEXT_SECONDARY }}>--set-bootstrap-password</code>{' '}
                    to start fresh; the old DB is preserved as{' '}
                    <code style={{ fontFamily: "var(--font-mono)", fontSize: 11, color: TEXT_SECONDARY }}>.db.bak</code> in case the hash turns up.
                  </div>
                </div>
                {recoveryError && <Alert type="error" message={recoveryError} showIcon />}
                <div>
                  <label style={{ fontSize: 13, fontWeight: 600, color: TEXT_SECONDARY, fontFamily: "var(--font-ui)", marginBottom: 6, display: 'block' }}>
                    Original Admin Password Hash
                  </label>
                  <Input.TextArea
                    value={recoveryPassword}
                    onChange={(e) => setRecoveryPassword(e.target.value)}
                    placeholder="$2b$12$... or base64-encoded hash"
                    autoFocus
                    rows={2}
                    className="dg-recovery-hash-input"
                    style={{ ...recoveryInputStyle, height: 'auto', fontSize: 13, fontFamily: "var(--font-mono)" }}
                  />
                </div>
                <Button
                  type="primary"
                  block
                  size="large"
                  loading={recoveryLoading}
                  disabled={!recoveryPassword.trim()}
                  onClick={handleRecover}
                  style={{ height: 48, borderRadius: 10, fontWeight: 700, fontFamily: "var(--font-ui)", fontSize: 15, letterSpacing: '0.02em', marginTop: 4 }}
                >
                  Try Hash
                </Button>
              </>
            )}
          </Space>
        </div>
      </div>
    );
  }

  return (
    <main className="dg-login-shell">
      <div className="dg-login-orb dg-login-orb-one" />
      <div className="dg-login-orb dg-login-orb-two" />
      <div className="dg-login-grid" />

      <section className="dg-login-card animate-fade-in" aria-label="DeltaGlider sign in">
        <Button
          type="text"
          shape="circle"
          size="small"
          icon={isDark ? <MoonOutlined /> : <SunOutlined />}
          title={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
          aria-label={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
          onClick={toggleTheme}
          className="dg-login-theme-toggle"
        />

        <Space direction="vertical" size={22} style={{ width: '100%' }}>
          <div className="dg-login-brand-block">
            <div className="dg-login-brand">DeltaGlider</div>
            <div className="dg-login-product">Proxy</div>
          </div>

          {isOpenSignedOutReconnect ? (
            <>
              {error && <Alert type="error" message={error} showIcon />}
              <Alert
                type="info"
                showIcon
                message="You signed out"
                description="This server uses open access. Connect again to return to the file browser, or open Settings from the menu after connecting if you need administrator tools."
              />
              <Button
                type="primary"
                block
                size="large"
                loading={loading}
                onClick={handleOpenReconnect}
                className="dg-login-submit"
              >
                Connect again
              </Button>
            </>
          ) : (
            <>
          {showError && !error && (
            <Alert type="warning" message="Stored credentials are invalid or the endpoint is unreachable." showIcon />
          )}
          {error && <Alert type="error" message={error} showIcon />}

          {/* OAuth provider buttons — shown prominently when available */}
          {externalProviders.length > 0 && (
            <OAuthProviderList
              providers={externalProviders}
              nextUrl={window.location.pathname}
              height={50}
              fontSize={14}
              variant="hero"
            />
          )}

          {/* Credential form — collapsible when OAuth is available */}
          {externalProviders.length > 0 && !showAdvanced ? (
            <div className="dg-login-secondary-action">
              <Button
                type="link"
                size="small"
                onClick={() => setShowAdvanced(true)}
                style={{ color: TEXT_MUTED, fontSize: 12, fontWeight: 700 }}
              >
                Sign in with credentials instead
              </Button>
            </div>
          ) : (
            <>
              {externalProviders.length > 0 && (
                <div className="dg-login-divider">
                  <span />
                  <Text style={{ color: TEXT_MUTED, fontSize: 11, fontWeight: 700 }}>or use credentials</Text>
                  <span />
                </div>
              )}

              {isBootstrap ? (
                /* Bootstrap mode: password only */
                <div>
                  <label className="dg-login-label">Admin password</label>
                  <Input.Password
                    value={adminPassword}
                    onChange={(e) => setAdminPassword(e.target.value)}
                    onPressEnter={handleConnect}
                    placeholder="Admin password"
                    size="large"
                    autoFocus={externalProviders.length === 0}
                    style={loginInputStyle}
                  />
                  <Text className="dg-login-hint">
                    For first-time setup and recovery
                  </Text>
                </div>
              ) : (
                /* IAM mode: access key + secret key */
                <div className="dg-login-fields">
                  <div>
                    <label className="dg-login-label">Access key ID</label>
                    <Input
                      value={accessKey}
                      onChange={(e) => setAccessKey(e.target.value)}
                      placeholder="Access Key ID"
                      size="large"
                      autoFocus={externalProviders.length === 0}
                      style={loginInputStyle}
                    />
                  </div>

                  <div>
                    <label className="dg-login-label">Secret access key</label>
                    <Input.Password
                      value={secretKey}
                      onChange={(e) => setSecretKey(e.target.value)}
                      onPressEnter={handleConnect}
                      placeholder="Secret Access Key"
                      size="large"
                      style={loginInputStyle}
                    />
                  </div>
                </div>
              )}

              <Button
                type="primary"
                block
                size="large"
                loading={loading}
                disabled={!canSubmit}
                onClick={handleConnect}
                className="dg-login-submit"
              >
                Sign in
              </Button>
            </>
          )}
            </>
          )}
        </Space>
      </section>
    </main>
  );
}
