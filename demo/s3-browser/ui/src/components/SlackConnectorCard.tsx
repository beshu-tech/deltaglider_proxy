/**
 * SlackConnectorCard — the `format: slack` editor for `advanced.event_delivery`.
 *
 * Renders INSIDE WebhookDeliveryPanel and edits the SAME `useSectionEditor`
 * value: the panel passes the live `form` down plus a `setField` patcher and the
 * per-instance `nextId` row-id counter. There is NO parallel state mirror here
 * (that's the documented admin-editor bug class) — every control reads from
 * `form` and writes through `setField`, so discard / dirty-dot / ⌘S stay correct.
 *
 * ## Two delivery modes (mode is DERIVED, but the operator picks which to set up)
 *
 * - **Incoming Webhook** (no bot token): a `hooks.slack.com/services/…` URL bound
 *   to one channel. Simplest; reuses the existing `webhook_urls` row editor.
 * - **Bot token** (`xoxb-…` set): posts via the Slack Web API to `slack_channel`;
 *   supports multiple channels + `@mentions`. Requires a channel.
 *
 * The mode toggle just clears/keeps the token so the derived backend mode follows
 * what the operator is editing. The masked bot-token field mirrors the header
 * secret UX: a server-masked token shows a "unchanged — type to replace"
 * placeholder; typing unmasks it (`slackBotTokenMasked → false`).
 *
 * No tooltips/popovers (globally disabled in this layout) — native `title=`. No
 * AntD Select popups (broken here) — checkboxes / Radio.Button only.
 */
import { useMemo, useState } from 'react';
import { Button, Checkbox, Input, Radio, Space, Typography } from 'antd';
import {
  CloseCircleFilled,
  DeleteOutlined,
  InfoCircleOutlined,
  PlusOutlined,
  SlackOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import SectionHeader from './SectionHeader';
import FormField from './FormField';
import { AdvancedDisclosure } from './ruleEditorFields';
import {
  SLACK_NOTIFY_KINDS,
  resolveSlackChannelsPreview,
  type WebhookFormState,
  type WebhookUrlRow,
  type SlackGlobRow,
  type SlackRouteRow,
} from './webhookDeliveryPayload';

const { Text } = Typography;

/** Which mode the operator is currently editing — derived from token presence. */
type SlackMode = 'webhook' | 'bot';

interface Props {
  form: WebhookFormState;
  setField: (patch: Partial<WebhookFormState>) => void;
  nextId: () => string;
  /** Live validation errors that belong to the Slack card (shown inline). */
  errors: string[];
  inputRadius: React.CSSProperties;
  /** Mutators for the shared webhook_urls rows (reused for the hooks.slack URL). */
  updateUrl: (id: string, url: string) => void;
  addUrl: () => void;
  removeUrl: (id: string) => void;
}

const APP_LINK = 'https://api.slack.com/apps?new_app=1';

export default function SlackConnectorCard({
  form,
  setField,
  nextId,
  errors,
  inputRadius,
  updateUrl,
  addUrl,
  removeUrl,
}: Props) {
  const colors = useColors();

  // Sample event for the routing preview. UI-only ephemeral state (not config):
  // lets the operator see which channel(s) a given bucket/key would resolve to.
  const [sampleBucket, setSampleBucket] = useState('releases');
  const [sampleKey, setSampleKey] = useState('builds/app.zip');

  // Display mode = the operator's UI choice (sticky even with an empty token
  // field). The BACKEND mode is derived from token presence at payload-build
  // time — leaving webhook mode clears the token so the two stay consistent.
  const mode: SlackMode = form.slackPreferBotMode ? 'bot' : 'webhook';

  const setMode = (next: SlackMode) => {
    if (next === mode) return;
    if (next === 'webhook') {
      // Leaving bot mode: drop the token so the backend resolves to webhook mode.
      setField({ slackPreferBotMode: false, slackBotToken: '', slackBotTokenMasked: false });
    } else {
      // Enter bot mode; the token field below captures the xoxb- value.
      setField({ slackPreferBotMode: true });
    }
  };

  // ── Glob row mutators (stable-id keyed, never array index) ──
  const updateGlob = (
    key: 'slackIncludeRows' | 'slackExcludeRows',
    id: string,
    glob: string,
  ) =>
    setField({
      [key]: form[key].map((r) => (r.id === id ? { ...r, glob } : r)),
    } as Partial<WebhookFormState>);
  const addGlob = (key: 'slackIncludeRows' | 'slackExcludeRows') =>
    setField({
      [key]: [...form[key], { id: nextId(), glob: '' } as SlackGlobRow],
    } as Partial<WebhookFormState>);
  const removeGlob = (key: 'slackIncludeRows' | 'slackExcludeRows', id: string) =>
    setField({
      [key]: form[key].filter((r) => r.id !== id),
    } as Partial<WebhookFormState>);

  // ── Channel-routing row mutators (stable-id keyed, never array index) ──
  const updateRoute = (id: string, patch: Partial<SlackRouteRow>) =>
    setField({
      slackRoutes: form.slackRoutes.map((r) => (r.id === id ? { ...r, ...patch } : r)),
    });
  const addRoute = () =>
    setField({
      slackRoutes: [
        ...form.slackRoutes,
        { id: nextId(), name: '', bucket: '', prefixGlobs: [], channel: '' } as SlackRouteRow,
      ],
    });
  const removeRoute = (id: string) =>
    setField({ slackRoutes: form.slackRoutes.filter((r) => r.id !== id) });

  // Nested glob-row mutators scoped to ONE route (stable-id keyed throughout).
  const updateRouteGlob = (routeId: string, globId: string, glob: string) =>
    setField({
      slackRoutes: form.slackRoutes.map((r) =>
        r.id === routeId
          ? { ...r, prefixGlobs: r.prefixGlobs.map((g) => (g.id === globId ? { ...g, glob } : g)) }
          : r,
      ),
    });
  const addRouteGlob = (routeId: string) =>
    setField({
      slackRoutes: form.slackRoutes.map((r) =>
        r.id === routeId
          ? { ...r, prefixGlobs: [...r.prefixGlobs, { id: nextId(), glob: '' } as SlackGlobRow] }
          : r,
      ),
    });
  const removeRouteGlob = (routeId: string, globId: string) =>
    setField({
      slackRoutes: form.slackRoutes.map((r) =>
        r.id === routeId
          ? { ...r, prefixGlobs: r.prefixGlobs.filter((g) => g.id !== globId) }
          : r,
      ),
    });

  const toggleKind = (kind: string, on: boolean) => {
    const set = new Set(form.slackNotifyKinds);
    if (on) set.add(kind);
    else set.delete(kind);
    // Preserve canonical ordering so the YAML diff stays stable.
    setField({ slackNotifyKinds: SLACK_NOTIFY_KINDS.filter((k) => set.has(k)) });
  };

  return (
    // No outer card here — the parent panel wraps this in the accent-bordered
    // "destination" card so the raw and Slack connectors share one frame.
    <>
      <SectionHeader
        icon={<SlackOutlined />}
        title="Slack connector"
        description="Post object events to a Slack channel as a formatted message. No restart — applies live."
      />

      {/* No-OAuth note — discreet one-liner, not a banner. */}
      <div
        style={{
          marginTop: 12,
          display: 'flex',
          alignItems: 'center',
          gap: 7,
          fontSize: 12,
          color: colors.TEXT_MUTED,
        }}
      >
        <InfoCircleOutlined style={{ color: colors.ACCENT_BLUE, fontSize: 13 }} />
        <span title="Delivery is outbound HTTPS only — Slack never needs to reach back to this proxy, so it works on private/internal instances.">
          No OAuth — just paste a credential. Works on private / internal instances.
        </span>
      </div>

      {errors.length > 0 && (
        <div
          style={{
            marginTop: 10,
            fontSize: 12,
            color: colors.ACCENT_RED,
            display: 'flex',
            alignItems: 'flex-start',
            gap: 7,
          }}
        >
          <CloseCircleFilled style={{ marginTop: 2, fontSize: 13 }} />
          <span>{errors.join(' · ')}</span>
        </div>
      )}

      {/* Mode sub-toggle */}
      <div style={{ marginTop: 16 }}>
        <FormField
          label="How to connect"
          helpText="Incoming Webhook is the quickest. Use a bot token when you need multiple channels or @mentions."
        >
          <Radio.Group
            value={mode}
            onChange={(e) => setMode(e.target.value as SlackMode)}
            style={{ display: 'flex', gap: 0 }}
          >
            <Radio.Button value="webhook" style={{ fontSize: 13 }} title="Post via a hooks.slack.com Incoming Webhook URL">
              Incoming Webhook (simplest)
            </Radio.Button>
            <Radio.Button value="bot" style={{ fontSize: 13 }} title="Post via the Slack Web API with a bot token">
              Bot token (multi-channel + @mentions)
            </Radio.Button>
          </Radio.Group>
        </FormField>
      </div>

      {mode === 'webhook' ? (
        <WebhookModeFields
          form={form}
          inputRadius={inputRadius}
          updateUrl={updateUrl}
          addUrl={addUrl}
          removeUrl={removeUrl}
          setField={setField}
          colors={colors}
        />
      ) : (
        <BotModeFields form={form} setField={setField} inputRadius={inputRadius} colors={colors} />
      )}

      {/* Channel routing — bot-token mode only (Incoming Webhook URLs are each
          bound to one channel by Slack, so per-bucket/prefix routing needs the
          Web API). Collapsed by default; the channel above is the fallback. */}
      {mode === 'bot' && (
        <AdvancedDisclosure title="Channel routing (per bucket / prefix)">
          <ChannelRoutingEditor
            form={form}
            inputRadius={inputRadius}
            colors={colors}
            updateRoute={updateRoute}
            addRoute={addRoute}
            removeRoute={removeRoute}
            updateRouteGlob={updateRouteGlob}
            addRouteGlob={addRouteGlob}
            removeRouteGlob={removeRouteGlob}
          />
        </AdvancedDisclosure>
      )}

      {/* What gets posted */}
      <AdvancedDisclosure title="What gets posted (event kinds + prefix filters)">
        <FormField
          label="Event kinds"
          yamlPath="advanced.event_delivery.slack_notify_kinds"
          helpText="Only these event kinds are posted to Slack. ObjectCreated is the default."
        >
          <Space direction="vertical" size={4} style={{ width: '100%' }}>
            {SLACK_NOTIFY_KINDS.map((kind) => (
              <Checkbox
                key={kind}
                checked={form.slackNotifyKinds.includes(kind)}
                onChange={(e) => toggleKind(kind, e.target.checked)}
              >
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>{kind}</span>
              </Checkbox>
            ))}
          </Space>
        </FormField>

        <GlobRowsField
          label="Include prefixes"
          yamlPath="advanced.event_delivery.slack_include_globs"
          helpText="Only keys matching at least one glob notify Slack. Empty = every key."
          rows={form.slackIncludeRows}
          onUpdate={(id, g) => updateGlob('slackIncludeRows', id, g)}
          onAdd={() => addGlob('slackIncludeRows')}
          onRemove={(id) => removeGlob('slackIncludeRows', id)}
          inputRadius={inputRadius}
          placeholder="releases/**"
        />
        <GlobRowsField
          label="Exclude prefixes"
          yamlPath="advanced.event_delivery.slack_exclude_globs"
          helpText="Keys matching any of these are never posted (exclude wins over include)."
          rows={form.slackExcludeRows}
          onUpdate={(id, g) => updateGlob('slackExcludeRows', id, g)}
          onAdd={() => addGlob('slackExcludeRows')}
          onRemove={(id) => removeGlob('slackExcludeRows', id)}
          inputRadius={inputRadius}
          placeholder="tmp/**"
        />
      </AdvancedDisclosure>

      {/* Live preview — ONE sample event drives both the resolved-channel chips
          AND the faux Slack message, so they never tell contradictory stories. */}
      <LivePreview
        form={form}
        mode={mode}
        colors={colors}
        inputRadius={inputRadius}
        sampleBucket={sampleBucket}
        sampleKey={sampleKey}
        onBucket={setSampleBucket}
        onKey={setSampleKey}
      />

      <Text type="secondary" style={{ fontSize: 11, display: 'block', marginTop: 12 }}>
        Need an app?{' '}
        <a href={APP_LINK} target="_blank" rel="noreferrer">
          Create a Slack app →
        </a>
      </Text>
    </>
  );
}

// ─────────────────────────────────────────────────────────────────────────
// Mode-specific field groups
// ─────────────────────────────────────────────────────────────────────────

function MiniGuide({ steps }: { steps: string[] }) {
  const { TEXT_MUTED, BORDER } = useColors();
  return (
    <ol
      style={{
        margin: '0 0 12px',
        paddingLeft: 0,
        listStyle: 'none',
        counterReset: 'slk',
        borderLeft: `2px solid ${BORDER}`,
      }}
    >
      {steps.map((s, i) => (
        <li
          key={i}
          style={{
            counterIncrement: 'slk',
            position: 'relative',
            paddingLeft: 28,
            marginBottom: 6,
            fontSize: 12,
            color: TEXT_MUTED,
            lineHeight: 1.4,
          }}
        >
          <span
            aria-hidden="true"
            style={{
              position: 'absolute',
              left: 8,
              top: 0,
              fontWeight: 700,
              fontFamily: 'var(--font-mono)',
            }}
          >
            {i + 1}.
          </span>
          {s}
        </li>
      ))}
    </ol>
  );
}

function WebhookModeFields({
  form,
  inputRadius,
  updateUrl,
  addUrl,
  removeUrl,
  setField,
  colors,
}: {
  form: WebhookFormState;
  inputRadius: React.CSSProperties;
  updateUrl: (id: string, url: string) => void;
  addUrl: () => void;
  removeUrl: (id: string) => void;
  setField: (patch: Partial<WebhookFormState>) => void;
  colors: ReturnType<typeof useColors>;
}) {
  return (
    <div style={{ marginTop: 8 }}>
      <MiniGuide
        steps={[
          'Create a Slack app (or open an existing one).',
          'Enable “Incoming Webhooks” and add one for your channel.',
          'Paste the hooks.slack.com webhook URL below.',
        ]}
      />
      <FormField
        label="Incoming Webhook URL"
        yamlPath="advanced.event_delivery.webhook_urls"
        helpText="The hooks.slack.com/services/… URL Slack generated. Each URL posts to its own bound channel."
      >
        <Space direction="vertical" style={{ width: '100%' }}>
          {form.urlRows.length === 0 && (
            <Text type="secondary" style={{ fontSize: 12 }}>
              No webhook URL yet. Add the one Slack gave you.
            </Text>
          )}
          {form.urlRows.map((row: WebhookUrlRow) => (
            <Space.Compact key={row.id} style={{ width: '100%' }}>
              <Input
                value={row.url}
                onChange={(e) => updateUrl(row.id, e.target.value)}
                placeholder="https://hooks.slack.com/services/T000/B000/xxxx"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
              />
              <Button
                icon={<DeleteOutlined />}
                onClick={() => removeUrl(row.id)}
                title="Remove webhook URL"
              />
            </Space.Compact>
          ))}
          <Button icon={<PlusOutlined />} onClick={addUrl} size="small">
            Add webhook URL
          </Button>
        </Space>
      </FormField>

      <FormField
        label="Sender name (optional)"
        yamlPath="advanced.event_delivery.slack_username"
        helpText="Overrides the display name the message posts as. Webhook mode only."
      >
        <Input
          value={form.slackUsername}
          onChange={(e) => setField({ slackUsername: e.target.value })}
          placeholder="DeltaGlider"
          style={{ ...inputRadius, fontSize: 13, maxWidth: 280 }}
        />
      </FormField>

      <FormField
        label="Icon emoji (optional)"
        yamlPath="advanced.event_delivery.slack_icon_emoji"
        helpText="A Slack emoji shortcode used as the avatar, e.g. :package:. Webhook mode only."
      >
        <Input
          value={form.slackIconEmoji}
          onChange={(e) => setField({ slackIconEmoji: e.target.value })}
          placeholder=":package:"
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 200, color: colors.TEXT_PRIMARY }}
        />
      </FormField>
    </div>
  );
}

function BotModeFields({
  form,
  setField,
  inputRadius,
  colors,
}: {
  form: WebhookFormState;
  setField: (patch: Partial<WebhookFormState>) => void;
  inputRadius: React.CSSProperties;
  colors: ReturnType<typeof useColors>;
}) {
  return (
    <div style={{ marginTop: 8 }}>
      <MiniGuide
        steps={[
          'Create a Slack app (or open an existing one).',
          'Add the chat:write and chat:write.public bot scopes.',
          'Install the app to your workspace.',
          'Paste the xoxb- bot token and the target channel below.',
        ]}
      />
      <FormField
        label="Bot token"
        yamlPath="advanced.event_delivery.slack_bot_token"
        helpText="The xoxb-… token from OAuth & Permissions. Stored encrypted and shown masked; leave it untouched to keep the current one."
      >
        <Input.Password
          value={form.slackBotTokenMasked ? '' : form.slackBotToken}
          onChange={(e) =>
            // Typing unmasks: it's now a real, operator-entered value.
            setField({ slackBotToken: e.target.value, slackBotTokenMasked: false })
          }
          placeholder={
            form.slackBotTokenMasked
              ? '•••••••• (unchanged — type to replace)'
              : 'xoxb-…'
          }
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 420, color: colors.TEXT_PRIMARY }}
        />
      </FormField>

      <FormField
        label="Channel"
        yamlPath="advanced.event_delivery.slack_channel"
        helpText="Channel id (like C0123ABC) or #name. Required in bot-token mode."
      >
        <Input
          value={form.slackChannel}
          onChange={(e) => setField({ slackChannel: e.target.value })}
          placeholder="#deploys or C0123ABC"
          style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 280 }}
        />
      </FormField>
    </div>
  );
}

function GlobRowsField({
  label,
  yamlPath,
  helpText,
  rows,
  onUpdate,
  onAdd,
  onRemove,
  inputRadius,
  placeholder,
}: {
  label: string;
  yamlPath?: string;
  helpText: string;
  rows: SlackGlobRow[];
  onUpdate: (id: string, glob: string) => void;
  onAdd: () => void;
  onRemove: (id: string) => void;
  inputRadius: React.CSSProperties;
  placeholder: string;
}) {
  return (
    <FormField label={label} yamlPath={yamlPath} helpText={helpText}>
      <Space direction="vertical" style={{ width: '100%' }}>
        {rows.length === 0 && (
          <Text type="secondary" style={{ fontSize: 12 }}>
            None.
          </Text>
        )}
        {rows.map((row) => (
          <Space.Compact key={row.id} style={{ width: '100%', maxWidth: 420 }}>
            <Input
              value={row.glob}
              onChange={(e) => onUpdate(row.id, e.target.value)}
              placeholder={placeholder}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
            />
            <Button icon={<DeleteOutlined />} onClick={() => onRemove(row.id)} title="Remove glob" />
          </Space.Compact>
        ))}
        <Button icon={<PlusOutlined />} onClick={onAdd} size="small">
          Add prefix glob
        </Button>
      </Space>
    </FormField>
  );
}

// ─────────────────────────────────────────────────────────────────────────
// Channel routing editor (bot-token mode only) — per bucket / prefix → channel.
// Stable-id row keys throughout (routes AND their nested glob rows), edited
// directly through the shared useSectionEditor value (no parallel state).
// ─────────────────────────────────────────────────────────────────────────

function ChannelRoutingEditor({
  form,
  inputRadius,
  colors,
  updateRoute,
  addRoute,
  removeRoute,
  updateRouteGlob,
  addRouteGlob,
  removeRouteGlob,
}: {
  form: WebhookFormState;
  inputRadius: React.CSSProperties;
  colors: ReturnType<typeof useColors>;
  updateRoute: (id: string, patch: Partial<SlackRouteRow>) => void;
  addRoute: () => void;
  removeRoute: (id: string) => void;
  updateRouteGlob: (routeId: string, globId: string, glob: string) => void;
  addRouteGlob: (routeId: string) => void;
  removeRouteGlob: (routeId: string, globId: string) => void;
}) {
  return (
    <div>
      <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: 12, lineHeight: 1.5 }}>
        Send different buckets or prefixes to different channels. An event posts
        to every route it matches; the channel above is the fallback for
        unmatched events.
      </Text>

      <Space direction="vertical" size={12} style={{ width: '100%' }}>
        {form.slackRoutes.length === 0 && (
          <Text type="secondary" style={{ fontSize: 12 }}>
            No routes — every event posts to the single channel above.
          </Text>
        )}
        {form.slackRoutes.map((route, i) => (
          <div
            key={route.id}
            style={{
              border: `1px solid ${colors.BORDER}`,
              borderRadius: 8,
              padding: 12,
              background: colors.BG_CARD,
            }}
          >
            <div
              style={{
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'space-between',
                marginBottom: 10,
              }}
            >
              <Text style={{ fontSize: 11, fontWeight: 700, letterSpacing: 0.5, textTransform: 'uppercase', color: colors.TEXT_MUTED }}>
                Route {i + 1}
              </Text>
              <Button
                icon={<DeleteOutlined />}
                size="small"
                onClick={() => removeRoute(route.id)}
                title="Remove route"
              />
            </div>

            <FormField label="Name (optional)">
              <Input
                value={route.name}
                onChange={(e) => updateRoute(route.id, { name: e.target.value })}
                placeholder="e.g. Releases → #ci"
                style={{ ...inputRadius, fontSize: 13, maxWidth: 320 }}
              />
            </FormField>

            <FormField label="Bucket">
              <Input
                value={route.bucket}
                onChange={(e) => updateRoute(route.id, { bucket: e.target.value })}
                placeholder="any bucket"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 280 }}
              />
            </FormField>

            <FormField
              label={
                <span>
                  Channel{' '}
                  <span style={{ color: colors.ACCENT_RED, fontWeight: 700 }} title="Required">
                    *
                  </span>
                </span>
              }
              helpText="Required. Channel id (C0123…) or #name."
            >
              <Input
                value={route.channel}
                onChange={(e) => updateRoute(route.id, { channel: e.target.value })}
                placeholder="C0123 or #name"
                status={route.channel.trim().length === 0 ? 'error' : undefined}
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 280 }}
              />
            </FormField>

            <GlobRowsField
              label="Prefix globs"
              helpText="Keys matching at least one glob route here. Empty = any key."
              rows={route.prefixGlobs}
              onUpdate={(globId, g) => updateRouteGlob(route.id, globId, g)}
              onAdd={() => addRouteGlob(route.id)}
              onRemove={(globId) => removeRouteGlob(route.id, globId)}
              inputRadius={inputRadius}
              placeholder="builds/** (empty = any key)"
            />
          </div>
        ))}
        <Button icon={<PlusOutlined />} onClick={addRoute} size="small">
          Add route
        </Button>
      </Space>
    </div>
  );
}

// ─────────────────────────────────────────────────────────────────────────
// Routing preview — resolve a SAMPLE bucket/key against the routes, mirroring
// the Rust resolve_channels (fan-out to all matches; fallback to the single
// channel; or nowhere). Best-effort client-side glob match.
// ─────────────────────────────────────────────────────────────────────────

function LivePreview({
  form,
  mode,
  colors,
  inputRadius,
  sampleBucket,
  sampleKey,
  onBucket,
  onKey,
}: {
  form: WebhookFormState;
  mode: SlackMode;
  colors: ReturnType<typeof useColors>;
  inputRadius: React.CSSProperties;
  sampleBucket: string;
  sampleKey: string;
  onBucket: (v: string) => void;
  onKey: (v: string) => void;
}) {
  const routed = mode === 'bot' && form.slackRoutes.length > 0;

  // Resolve the SAMPLE event to its channel(s) once — drives both the chips and
  // the faux message below, so they always agree.
  const resolved = useMemo(
    () => resolveSlackChannelsPreview(form.slackRoutes, form.slackChannel, sampleBucket, sampleKey),
    [form.slackRoutes, form.slackChannel, sampleBucket, sampleKey],
  );

  // The channel the faux message shows. In webhook mode the message goes to the
  // webhook's bound channel; in bot mode it's the resolved channel(s) for the
  // sample, the fallback (no routes), or "no channel" (routed but unmatched).
  let messageChannels: string[];
  if (mode === 'webhook') {
    messageChannels = ['webhook channel'];
  } else if (resolved.matches.length > 0) {
    messageChannels = resolved.matches.map((m) => fmtChannel(m.channel));
  } else if (resolved.fellBackToChannel) {
    messageChannels = [fmtChannel(resolved.fallbackChannel)];
  } else if (!routed) {
    messageChannels = [fmtChannel(form.slackChannel) || '#your-channel'];
  } else {
    messageChannels = []; // routed but unmatched → posts nowhere
  }

  const chipBase: React.CSSProperties = {
    fontFamily: 'var(--font-mono)',
    fontSize: 12,
    borderRadius: 4,
    padding: '1px 7px',
    border: `1px solid ${colors.BORDER}`,
  };

  return (
    <div style={{ marginTop: 20 }}>
      <Text
        type="secondary"
        style={{
          fontSize: 11,
          fontWeight: 700,
          letterSpacing: 0.5,
          textTransform: 'uppercase',
          display: 'block',
          marginBottom: 8,
        }}
      >
        Live preview — what lands in the channel
      </Text>

      {/* One sample event input (bot mode) → resolved-channel chips. */}
      {mode === 'bot' && (
        <div
          style={{
            marginBottom: 12,
            padding: 12,
            border: `1px dashed ${colors.BORDER}`,
            borderRadius: 8,
          }}
        >
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, display: 'block', marginBottom: 8 }}>
            Try a sample event{routed ? '' : ' (add routes to send buckets to different channels)'}:
          </Text>
          <Space.Compact style={{ width: '100%', maxWidth: 420, marginBottom: 10 }}>
            <Input
              value={sampleBucket}
              onChange={(e) => onBucket(e.target.value)}
              placeholder="bucket"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13, maxWidth: 150 }}
              title="Sample bucket"
            />
            <Input
              value={sampleKey}
              onChange={(e) => onKey(e.target.value)}
              placeholder="path/to/key.zip"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 13 }}
              title="Sample object key"
            />
          </Space.Compact>
          <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap', fontSize: 13 }}>
            <span style={{ color: colors.TEXT_MUTED }}>→</span>
            {resolved.matches.length > 0 ? (
              resolved.matches.map((m) => (
                <span
                  key={m.channel}
                  style={{ ...chipBase, color: colors.ACCENT_BLUE }}
                  title={`Matched route: ${m.label}`}
                >
                  {fmtChannel(m.channel)}
                </span>
              ))
            ) : resolved.fellBackToChannel ? (
              <span style={{ ...chipBase, color: colors.TEXT_SECONDARY }} title="No route matched — fallback channel">
                (fallback) {fmtChannel(resolved.fallbackChannel)}
              </span>
            ) : routed ? (
              <span style={{ ...chipBase, color: colors.ACCENT_RED }} title="No route matched and no fallback — this event would be posted NOWHERE">
                (no channel — not posted)
              </span>
            ) : (
              <span style={{ ...chipBase, color: colors.TEXT_SECONDARY }}>
                {fmtChannel(form.slackChannel) || '#your-channel'}
              </span>
            )}
          </div>
        </div>
      )}

      {/* The faux Slack message — same sample + same resolved channel. */}
      <SlackMessagePreview
        form={form}
        mode={mode}
        colors={colors}
        sampleBucket={sampleBucket || 'my-bucket'}
        sampleKey={sampleKey || 'releases/app-v1.2.0.tar.gz'}
        channels={messageChannels}
      />
    </div>
  );
}

/** Display a channel as `#name` / `C0123…` (prefix-aware), like Slack shows it. */
function fmtChannel(c: string): string {
  const t = c.trim();
  if (!t) return '';
  return t.startsWith('#') || t.startsWith('C') ? t : `#${t}`;
}

// ─────────────────────────────────────────────────────────────────────────
// Live faux-Slack preview — rendered purely client-side from current settings.
// ─────────────────────────────────────────────────────────────────────────

function SlackMessagePreview({
  form,
  mode,
  colors,
  sampleBucket,
  sampleKey,
  channels,
}: {
  form: WebhookFormState;
  mode: SlackMode;
  colors: ReturnType<typeof useColors>;
  sampleBucket: string;
  sampleKey: string;
  /** Resolved channel label(s) for the sample. Empty = posts nowhere. */
  channels: string[];
}) {
  // Slack's surfaces are always light; render a fixed light card so the preview
  // reads as "this is what Slack shows", independent of the admin theme.
  const appName =
    mode === 'webhook' && form.slackUsername.trim() ? form.slackUsername.trim() : 'DeltaGlider';
  const kind = form.slackNotifyKinds[0] ?? 'ObjectCreated';
  const channelLabel =
    channels.length === 0 ? '(not posted)' : channels.join(', ');

  const verb =
    kind === 'ObjectDeleted'
      ? 'Deleted'
      : kind === 'ObjectCopied' || kind === 'ReplicationObjectCopied'
        ? 'Copied'
        : kind === 'LifecycleExpired'
          ? 'Expired'
          : kind === 'LifecycleTransitioned'
            ? 'Transitioned'
            : 'New object';

  return (
    <div
      style={{
        background: '#ffffff',
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 10,
        padding: 14,
        display: 'flex',
        gap: 10,
        fontFamily:
          'Slack-Lato, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif',
        color: '#1d1c1d',
        boxShadow: '0 1px 2px rgba(0,0,0,0.06)',
      }}
    >
      {/* Avatar */}
      <div
        style={{
          width: 36,
          height: 36,
          borderRadius: 8,
          background: '#4a154b',
          color: '#fff',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          fontSize: 18,
          flexShrink: 0,
        }}
        aria-hidden="true"
      >
        📦
      </div>
      <div style={{ minWidth: 0, flex: 1 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 2 }}>
          <span style={{ fontWeight: 700, fontSize: 13 }}>{appName}</span>
          <span
            style={{
              fontSize: 10,
              background: '#e8e8e8',
              color: '#616061',
              borderRadius: 3,
              padding: '0 4px',
              fontWeight: 700,
            }}
          >
            APP
          </span>
          <span style={{ fontSize: 11, color: '#616061' }}>just now</span>
        </div>
        <div style={{ fontSize: 13, lineHeight: 1.45 }}>
          <div style={{ fontWeight: 700, marginBottom: 2 }}>
            📦 {verb} in <span style={{ color: '#1264a3' }}>{channelLabel}</span>
          </div>
          <div>
            <span style={{ fontWeight: 700 }}>{sampleBucket}</span>{' '}
            <code
              style={{
                background: '#f6f6f6',
                border: '1px solid #e0e0e0',
                borderRadius: 3,
                padding: '0 4px',
                fontSize: 12,
                color: '#c0392b',
              }}
            >
              {sampleKey}
            </code>
          </div>
          <div style={{ color: '#616061', fontSize: 12, marginTop: 2 }}>2.4 MB · application/gzip</div>
        </div>
      </div>
    </div>
  );
}
