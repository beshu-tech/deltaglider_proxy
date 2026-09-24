/**
 * SlackSetupGuide — an illustrated, step-by-step "how to create a Slack app"
 * guide. The CONTENT (step cards + schematic Slack mockups) now lives in
 * `SlackSetupGuideContent`, which the connector card opens in a roomy AntD
 * `Drawer` (`SlackSetupGuideDrawer`) instead of cramming it inline. At drawer
 * width (~560px) the mockups finally have breathing room.
 *
 * Two flows (webhook / bot token) share one StepCard primitive. The "mockups"
 * are pure CSS/SVG fac-similes of the relevant Slack settings screens (NO
 * external images — they'd break offline / under a strict CSP and could rot
 * against Slack's real UI). They're deliberately schematic: enough to orient a
 * first-time operator to where the button/toggle lives, not pixel-perfect.
 *
 * Copy-buttons surface the exact values the operator must paste into Slack
 * (the scopes), so they don't have to transcribe them.
 */
import { Button, Drawer, Typography } from 'antd';
import { CheckOutlined, CopyOutlined, SlackOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useCopyToClipboard } from '../useCopyToClipboard';

const { Text } = Typography;

const SLACK_APPS_URL = 'https://api.slack.com/apps?new_app=1';

type Mode = 'webhook' | 'bot';

/**
 * Roomy slide-in guide. The parent owns `open`/`onClose` (UI-only local state)
 * and which `mode` to show; this just frames the step-by-step content with
 * generous padding. At drawer width (~560px) the schematic Slack mockups
 * finally have room — the whole point of moving them out of the inline column.
 */
export function SlackSetupGuideDrawer({
  open,
  mode,
  onClose,
}: {
  open: boolean;
  mode: Mode;
  onClose: () => void;
}) {
  const c = useColors();
  return (
    <Drawer
      open={open}
      onClose={onClose}
      width={Math.min(640, typeof window !== 'undefined' ? window.innerWidth - 32 : 640)}
      title={
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 10, fontSize: 16 }}>
          <SlackOutlined style={{ color: '#611f69', fontSize: 20 }} />
          Set up a Slack app
          <span
            style={{
              fontSize: 11,
              fontWeight: 700,
              letterSpacing: 0.5,
              textTransform: 'uppercase',
              color: c.TEXT_MUTED,
              border: `1px solid ${c.BORDER}`,
              borderRadius: 5,
              padding: '1px 7px',
            }}
          >
            {mode === 'webhook' ? 'Incoming Webhook' : 'Bot token'}
          </span>
        </span>
      }
      styles={{ body: { padding: '24px 28px 40px' } }}
    >
      <Text style={{ fontSize: 13, color: c.TEXT_MUTED, display: 'block', marginBottom: 24, lineHeight: 1.6 }}>
        One-time, ~2 minutes. No OAuth callback — you create the app in Slack and paste a
        credential back here. Nothing needs to reach this proxy.
      </Text>
      {mode === 'webhook' ? <WebhookFlow c={c} /> : <BotFlow c={c} />}
    </Drawer>
  );
}

// ── Shared step card ─────────────────────────────────────────────────────────

function StepCard({
  n,
  title,
  children,
  mockup,
  c,
}: {
  n: number;
  title: React.ReactNode;
  children?: React.ReactNode;
  mockup?: React.ReactNode;
  c: ReturnType<typeof useColors>;
}) {
  return (
    <div style={{ display: 'flex', gap: 16, marginBottom: 24 }}>
      {/* Number column with connecting rail */}
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'center' }}>
        <div
          style={{
            width: 30,
            height: 30,
            borderRadius: '50%',
            background: c.ACCENT_BLUE,
            color: '#fff',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            fontSize: 14,
            fontWeight: 700,
            flexShrink: 0,
          }}
        >
          {n}
        </div>
        <div style={{ flex: 1, width: 2, background: c.BORDER, marginTop: 6 }} />
      </div>
      <div style={{ flex: 1, minWidth: 0, paddingBottom: 4 }}>
        <div style={{ fontSize: 15, fontWeight: 600, color: c.TEXT_PRIMARY, marginBottom: 6 }}>
          {title}
        </div>
        {children && (
          <div style={{ fontSize: 13.5, color: c.TEXT_SECONDARY, lineHeight: 1.6 }}>{children}</div>
        )}
        {mockup && <div style={{ marginTop: 12 }}>{mockup}</div>}
      </div>
    </div>
  );
}

function CopyChip({ value, c }: { value: string; c: ReturnType<typeof useColors> }) {
  const { copy, copied } = useCopyToClipboard();
  return (
    <button
      onClick={() => void copy(value, { successMessage: `Copied ${value}`, resetMs: 1500 })}
      title={`Copy "${value}"`}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        fontFamily: 'var(--font-mono)',
        fontSize: 13,
        padding: '4px 10px',
        borderRadius: 6,
        border: `1px solid ${c.BORDER}`,
        background: c.BG_CARD,
        color: c.ACCENT_BLUE,
        cursor: 'pointer',
        marginRight: 8,
        marginTop: 6,
      }}
    >
      {value}
      {copied ? <CheckOutlined style={{ fontSize: 10 }} /> : <CopyOutlined style={{ fontSize: 10 }} />}
    </button>
  );
}

function CreateAppButton() {
  return (
    <a href={SLACK_APPS_URL} target="_blank" rel="noreferrer" style={{ display: 'inline-block', marginTop: 10 }}>
      <Button icon={<SlackOutlined />}>Open Slack app builder ↗</Button>
    </a>
  );
}

// ── Faux Slack-UI mockups (schematic, CSS only) ──────────────────────────────

/** A small framed "Slack settings screen" wrapper. */
function SlackScreen({
  title,
  children,
  c,
}: {
  title: string;
  children: React.ReactNode;
  c: ReturnType<typeof useColors>;
}) {
  return (
    <div
      style={{
        border: `1px solid ${c.BORDER}`,
        borderRadius: 8,
        overflow: 'hidden',
        maxWidth: 460,
        background: '#fff',
        color: '#1d1c1d',
        fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
        boxShadow: '0 1px 3px rgba(0,0,0,0.08)',
      }}
    >
      <div
        style={{
          background: '#3f0e40',
          color: '#fff',
          padding: '8px 14px',
          fontSize: 12,
          fontWeight: 600,
          display: 'flex',
          alignItems: 'center',
          gap: 7,
        }}
      >
        <SlackOutlined style={{ fontSize: 13 }} /> {title}
      </div>
      <div style={{ padding: 14 }}>{children}</div>
    </div>
  );
}

function SlackBtn({ children, primary = true }: { children: React.ReactNode; primary?: boolean }) {
  return (
    <span
      style={{
        display: 'inline-block',
        background: primary ? '#007a5a' : '#fff',
        color: primary ? '#fff' : '#1d1c1d',
        border: primary ? 'none' : '1px solid #ddd',
        borderRadius: 4,
        padding: '4px 12px',
        fontSize: 12,
        fontWeight: 700,
      }}
    >
      {children}
    </span>
  );
}

function ScopeRow({ name }: { name: string }) {
  return (
    <div
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '7px 10px',
        borderRadius: 4,
        background: '#f8f8f8',
        marginBottom: 6,
        fontSize: 13,
      }}
    >
      <code style={{ color: '#1264a3', fontWeight: 600 }}>{name}</code>
      <span style={{ color: '#007a5a', fontSize: 12, fontWeight: 700 }}>+ Add</span>
    </div>
  );
}

function Toggle({ on = true }: { on?: boolean }) {
  return (
    <span
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        width: 34,
        height: 18,
        borderRadius: 10,
        background: on ? '#007a5a' : '#ccc',
        padding: 2,
        justifyContent: on ? 'flex-end' : 'flex-start',
      }}
      aria-hidden="true"
    >
      <span style={{ width: 14, height: 14, borderRadius: '50%', background: '#fff' }} />
    </span>
  );
}

// ── The two flows ────────────────────────────────────────────────────────────

function WebhookFlow({ c }: { c: ReturnType<typeof useColors> }) {
  return (
    <div>
      <StepCard n={1} title="Create a Slack app" c={c}>
        Go to the Slack app builder and click <b>Create New App → From scratch</b>. Name it (e.g.
        “DeltaGlider”) and pick your workspace.
        <div>
          <CreateAppButton />
        </div>
        <SlackScreen title="Create an app" c={c}>
          <div style={{ fontSize: 12, marginBottom: 8 }}>Pick a name &amp; workspace</div>
          <div
            style={{
              border: '1px solid #ddd',
              borderRadius: 4,
              padding: '5px 8px',
              fontSize: 12,
              color: '#1d1c1d',
              marginBottom: 8,
            }}
          >
            DeltaGlider
          </div>
          <SlackBtn>Create App</SlackBtn>
        </SlackScreen>
      </StepCard>

      <StepCard n={2} title="Enable Incoming Webhooks" c={c}>
        In the left sidebar open <b>Incoming Webhooks</b> and flip <b>Activate Incoming Webhooks</b>{' '}
        to On.
        <SlackScreen title="Incoming Webhooks" c={c}>
          <div
            style={{
              display: 'flex',
              alignItems: 'center',
              justifyContent: 'space-between',
              fontSize: 12,
            }}
          >
            <span>Activate Incoming Webhooks</span>
            <Toggle on />
          </div>
        </SlackScreen>
      </StepCard>

      <StepCard n={3} title="Add a webhook to a channel" c={c}>
        Click <b>Add New Webhook to Workspace</b>, choose the channel to post to, and <b>Allow</b>.
        Slack creates a webhook URL bound to that one channel.
        <SlackScreen title="Add Webhook → choose channel" c={c}>
          <div style={{ fontSize: 12, marginBottom: 8 }}>
            Post to <code style={{ color: '#1264a3' }}>#deploys</code>
          </div>
          <SlackBtn>Allow</SlackBtn>
        </SlackScreen>
      </StepCard>

      <StepCard
        n={4}
        title="Copy the Webhook URL → paste below"
        c={c}
      >
        Copy the generated{' '}
        <code style={{ color: c.ACCENT_BLUE }}>https://hooks.slack.com/services/…</code> URL and
        paste it into the <b>Endpoints</b> field below. Done — that channel now gets your events.
      </StepCard>
    </div>
  );
}

function BotFlow({ c }: { c: ReturnType<typeof useColors> }) {
  return (
    <div>
      <StepCard n={1} title="Create a Slack app" c={c}>
        Open the Slack app builder → <b>Create New App → From scratch</b>, name it, pick your
        workspace.
        <div>
          <CreateAppButton />
        </div>
      </StepCard>

      <StepCard n={2} title="Add bot token scopes" c={c}>
        Open <b>OAuth &amp; Permissions</b> → under <b>Bot Token Scopes</b> add these two (copy them):
        <div style={{ marginBottom: 4 }}>
          <CopyChip value="chat:write" c={c} />
          <CopyChip value="chat:write.public" c={c} />
        </div>
        <span style={{ fontSize: 11, color: c.TEXT_MUTED }}>
          <code>chat:write.public</code> lets the bot post to any public channel without being
          invited.
        </span>
        <SlackScreen title="OAuth &amp; Permissions → Bot Token Scopes" c={c}>
          <ScopeRow name="chat:write" />
          <ScopeRow name="chat:write.public" />
        </SlackScreen>
      </StepCard>

      <StepCard n={3} title="Install to your workspace" c={c}>
        Scroll up and click <b>Install to Workspace</b>, then <b>Allow</b>.
        <SlackScreen title="OAuth &amp; Permissions" c={c}>
          <SlackBtn>Install to Workspace</SlackBtn>
        </SlackScreen>
      </StepCard>

      <StepCard n={4} title="Copy the Bot User OAuth Token → paste below" c={c}>
        After installing, copy the <b>Bot User OAuth Token</b> (starts with{' '}
        <code style={{ color: c.ACCENT_BLUE }}>xoxb-</code>) and paste it into <b>Bot token</b> below.
        Then set the target <b>Channel</b> (a channel id like <code>C0123ABC</code> or{' '}
        <code>#name</code>).
        <SlackScreen title="OAuth Tokens for Your Workspace" c={c}>
          <div style={{ fontSize: 11, color: '#616061', marginBottom: 4 }}>Bot User OAuth Token</div>
          <div
            style={{
              fontFamily: 'monospace',
              fontSize: 12,
              background: '#f8f8f8',
              border: '1px solid #ddd',
              borderRadius: 4,
              padding: '5px 8px',
              color: '#1d1c1d',
            }}
          >
            xoxb-2-•••••••••••••
          </div>
        </SlackScreen>
      </StepCard>

      <StepCard n={5} title="Invite the bot (only for PRIVATE channels)" c={c}>
        Public channels work automatically via <code>chat:write.public</code>. For a <b>private</b>{' '}
        channel, type <code>/invite @YourApp</code> in that channel once.
      </StepCard>
    </div>
  );
}
