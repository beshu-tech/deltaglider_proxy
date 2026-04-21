/**
 * CommandPalette — Wave 10, §10.3 of the admin UI revamp plan.
 *
 * `⌘K` / `Ctrl+K` opens a modal with a fuzzy filter over every
 * admin page + a handful of global actions (Export YAML, Import
 * YAML, Setup wizard, Sign out). Keyboard-first navigation: up /
 * down arrows to move, Enter to activate, Esc to close.
 *
 * Rationale: the four-group sidebar already scales to ~17 entries,
 * but search is faster for operators who know what they want. The
 * palette is the canonical "quick nav" pattern from Cloudflare /
 * Vercel / Linear / Slack — minimum table stakes for a modern
 * admin console.
 *
 * Mounts a listener on `window.keydown` at mount; unmount cleans
 * up. No data fetching — the command list is static (derived from
 * ADMIN_IA) + a few shell actions.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { Input, Modal, Typography } from 'antd';
import {
  SearchOutlined,
  FileTextOutlined,
  ImportOutlined,
  RocketOutlined,
  LogoutOutlined,
  QuestionCircleOutlined,
} from '@ant-design/icons';
import type { ReactNode } from 'react';
import { useColors } from '../ThemeContext';
import { ADMIN_IA } from './AdminSidebar';

const { Text } = Typography;

export interface CommandAction {
  id: string;
  label: string;
  hint?: string;
  /** Keywords beyond the visible label that the fuzzy filter should match. */
  keywords?: string;
  icon: ReactNode;
  onRun: () => void;
  /** Optional shortcut string to render on the right side. */
  shortcut?: string;
}

interface Props {
  open: boolean;
  onClose: () => void;
  /** Navigate to an admin sub-path. The palette passes this the path
   *  as-is (no leading `/admin/`). */
  onNavigateAdmin: (path: string) => void;
  /** Extra actions the shell owns (e.g. "Export YAML" opens a modal). */
  extraActions?: CommandAction[];
}

/** Flatten ADMIN_IA into a flat list of nav commands. */
function buildNavCommands(
  onNavigateAdmin: (path: string) => void
): CommandAction[] {
  const out: CommandAction[] = [];
  for (const group of ADMIN_IA) {
    for (const entry of group.entries) {
      out.push({
        id: `nav:${entry.path}`,
        label: entry.label,
        hint: `Go to ${group.group} → ${entry.label}`,
        keywords: `${group.group} ${entry.label} ${entry.path}`,
        icon: entry.icon,
        onRun: () => onNavigateAdmin(entry.path),
      });
      if (entry.children) {
        for (const child of entry.children) {
          out.push({
            id: `nav:${child.path}`,
            label: `${entry.label} → ${child.label}`,
            hint: `Go to ${group.group} → ${entry.label} → ${child.label}`,
            keywords: `${group.group} ${entry.label} ${child.label} ${child.path}`,
            icon: child.icon,
            onRun: () => onNavigateAdmin(child.path),
          });
        }
      }
    }
  }
  return out;
}

/** Case-insensitive subsequence match + letter-start bonus. Small
 *  hand-rolled filter — no dependency needed for <50 entries. */
function scoreMatch(needle: string, haystack: string): number {
  if (!needle) return 1;
  const n = needle.toLowerCase();
  const h = haystack.toLowerCase();
  if (h.includes(n)) {
    // Exact substring: very high score, boosted if prefix.
    return h.startsWith(n) ? 1000 : 500 + (100 - h.length);
  }
  // Subsequence match (every char of n appears in h in order).
  let i = 0;
  for (const ch of h) {
    if (ch === n[i]) i += 1;
    if (i === n.length) break;
  }
  return i === n.length ? 100 + (50 - h.length) : 0;
}

export default function CommandPalette({
  open,
  onClose,
  onNavigateAdmin,
  extraActions,
}: Props) {
  const colors = useColors();
  const [query, setQuery] = useState('');
  const [cursor, setCursor] = useState(0);
  const inputRef = useRef<import('antd').InputRef>(null);

  const commands = useMemo(
    () => [...buildNavCommands(onNavigateAdmin), ...(extraActions ?? [])],
    [onNavigateAdmin, extraActions]
  );
  const filtered = useMemo(() => {
    const scored = commands
      .map((c) => ({
        c,
        score: Math.max(
          scoreMatch(query, c.label),
          scoreMatch(query, c.keywords ?? '')
        ),
      }))
      .filter((x) => x.score > 0)
      .sort((a, b) => b.score - a.score);
    return scored.map((x) => x.c);
  }, [commands, query]);

  // Reset query + cursor whenever the modal opens.
  useEffect(() => {
    if (!open) return;
    setQuery('');
    setCursor(0);
    // Focus the search field on next frame (AntD mounts lazily).
    // Clear the timeout on close / unmount to avoid a zombie focus
    // call targeting a disposed input.
    const t = window.setTimeout(() => inputRef.current?.focus(), 50);
    return () => window.clearTimeout(t);
  }, [open]);

  // Clamp cursor inside the filtered list.
  useEffect(() => {
    if (cursor >= filtered.length) setCursor(Math.max(0, filtered.length - 1));
  }, [filtered.length, cursor]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setCursor((c) => Math.min(c + 1, filtered.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setCursor((c) => Math.max(c - 1, 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const pick = filtered[cursor];
      if (pick) {
        pick.onRun();
        onClose();
      }
    } else if (e.key === 'Escape') {
      onClose();
    }
  };

  return (
    <Modal
      open={open}
      onCancel={onClose}
      footer={null}
      closable={false}
      destroyOnHidden
      width={640}
      styles={{
        body: {
          padding: 0,
          maxHeight: '70vh',
          overflow: 'hidden',
          display: 'flex',
          flexDirection: 'column',
        },
      }}
    >
      <div style={{ padding: '14px 16px', borderBottom: `1px solid ${colors.BORDER}` }}>
        <Input
          ref={inputRef}
          size="large"
          placeholder="Type to filter pages or actions..."
          prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setCursor(0);
          }}
          onKeyDown={onKeyDown}
          autoFocus
          variant="borderless"
          style={{ fontSize: 16 }}
          // Accessibility: announce the cursor-highlighted row to
          // screen readers as the "active descendant" of the combobox.
          role="combobox"
          aria-expanded
          aria-controls="command-palette-listbox"
          aria-activedescendant={
            filtered[cursor] ? `cmd-${filtered[cursor].id}` : undefined
          }
          aria-autocomplete="list"
        />
      </div>
      <div
        id="command-palette-listbox"
        role="listbox"
        aria-label="Command palette"
        style={{
          overflowY: 'auto',
          flex: 1,
          padding: 6,
        }}
      >
        {filtered.length === 0 ? (
          <div
            style={{
              padding: 40,
              textAlign: 'center',
              color: colors.TEXT_MUTED,
              fontSize: 13,
            }}
          >
            No matches. Try a shorter query.
          </div>
        ) : (
          filtered.map((c, i) => (
            <CommandRow
              key={c.id}
              command={c}
              active={i === cursor}
              onHover={() => setCursor(i)}
              onClick={() => {
                c.onRun();
                onClose();
              }}
            />
          ))
        )}
      </div>
      <div
        style={{
          borderTop: `1px solid ${colors.BORDER}`,
          padding: '8px 16px',
          display: 'flex',
          justifyContent: 'space-between',
          fontSize: 11,
          color: colors.TEXT_MUTED,
          fontFamily: 'var(--font-ui)',
        }}
      >
        <span>
          <kbd>↑</kbd> <kbd>↓</kbd> navigate · <kbd>Enter</kbd> run ·{' '}
          <kbd>Esc</kbd> close
        </span>
        <span>
          {filtered.length} result{filtered.length === 1 ? '' : 's'}
        </span>
      </div>
    </Modal>
  );
}

function CommandRow({
  command,
  active,
  onHover,
  onClick,
}: {
  command: CommandAction;
  active: boolean;
  onHover: () => void;
  onClick: () => void;
}) {
  const colors = useColors();
  return (
    <button
      // Stable id so the combobox Input can announce this row as
      // its aria-activedescendant when the cursor points at it.
      id={`cmd-${command.id}`}
      role="option"
      aria-selected={active}
      onMouseEnter={onHover}
      onClick={onClick}
      style={{
        display: 'flex',
        alignItems: 'center',
        gap: 12,
        width: '100%',
        padding: '10px 12px',
        border: 'none',
        background: active ? `${colors.ACCENT_BLUE}15` : 'transparent',
        borderRadius: 8,
        cursor: 'pointer',
        textAlign: 'left',
        color: colors.TEXT_PRIMARY,
        fontFamily: 'var(--font-ui)',
        fontSize: 14,
        transition: 'background 0.12s',
      }}
    >
      <span
        style={{
          width: 28,
          height: 28,
          borderRadius: 6,
          background: colors.BG_ELEVATED,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          color: active ? colors.ACCENT_BLUE : colors.TEXT_SECONDARY,
          flexShrink: 0,
        }}
      >
        {command.icon}
      </span>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontWeight: 600 }}>{command.label}</div>
        {command.hint && (
          <div
            style={{
              fontSize: 11,
              color: colors.TEXT_MUTED,
              marginTop: 1,
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}
          >
            {command.hint}
          </div>
        )}
      </div>
      {command.shortcut && (
        <Text
          style={{
            fontFamily: 'var(--font-mono)',
            fontSize: 11,
            color: colors.TEXT_MUTED,
          }}
        >
          {command.shortcut}
        </Text>
      )}
    </button>
  );
}

// Re-export convenience icons so consumers building extra actions
// don't need to pull from antd themselves.
export { FileTextOutlined, ImportOutlined, RocketOutlined, LogoutOutlined, QuestionCircleOutlined };
