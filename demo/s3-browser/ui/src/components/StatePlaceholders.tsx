/**
 * StatePlaceholders — THE state system of the UI (UX harvest D16, D17, D35).
 *
 * A data view is in exactly one of four states. Use these components and
 * nothing else for the first three; the source test
 * `stateSystem.test.ts` refuses the stock AntD <Empty> and hand-written
 * "No … yet" placeholders outside this file.
 *
 *   loading → <LoadingState variant="list" | "card" | "form" | "inline" />
 *             A skeleton in the SHAPE of what will come (rows of a list, a
 *             card, label + field pairs), so the page does not jump. Never
 *             the empty state or stale rows while loading. "inline" is a
 *             small spinner for a computation inside a drawer or a modal.
 *   error   → <ErrorState error={err} what="the audit log" onRetry={refetch} />
 *             "Could not load the audit log", ONE plain sentence from
 *             `plainError`, a Retry button and a "Details" toggle with the
 *             raw text. A page with N failing queries shows ONE.
 *   empty   → <EmptyState icon title hint action secondaryAction />
 *             Icon, title, one sentence, at most a primary and a secondary
 *             action (rendered <Button>s: type="primary" and default).
 *   data    → the page.
 *
 * Let <QueryState> decide, so error and empty can never show together:
 *
 *   <QueryState
 *     query={audit}                         // react-query result, or { data, error, isPending }
 *     what="the audit log"
 *     loading={<LoadingState variant="list" rows={6} />}   // optional; list by default
 *     isEmpty={(d) => d.entries.length === 0}
 *     empty={<EmptyState icon={<FileTextOutlined />} title="No audit entries yet" hint="…" />}
 *   >
 *     {(data) => <AuditTable entries={data.entries} />}
 *   </QueryState>
 *
 * Several queries: `queries={[a, b]}` → `children([dataA, dataB])`.
 */
import { useState, type ReactNode } from 'react';
import { Button, Skeleton, Spin } from 'antd';
import { ExclamationCircleOutlined, ReloadOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { plainError } from '../plainError';
import { queriesView, queryView, type QueryLike } from '../queryView';
import { CARD_PADDING, CARD_RADIUS, SPACE } from './shared-styles';

export type LoadingVariant = 'list' | 'card' | 'form' | 'inline';

/**
 * A skeleton in the shape of the content that is loading.
 *  - list:   `rows` rows of one line each (a table, a master list).
 *  - card:   a bordered card with a title bar and `rows` text lines.
 *  - form:   `rows` label + field pairs.
 *  - inline: a small spinner and `label` (a preview, a computation).
 * `label` is the accessible name ("Loading users"); only "inline" shows it.
 */
export function LoadingState({
  variant = 'list',
  rows,
  label = 'Loading',
}: {
  variant?: LoadingVariant;
  rows?: number;
  label?: string;
}) {
  const { BG_CARD, BORDER, TEXT_MUTED } = useColors();
  if (variant === 'inline') {
    return (
      <div
        role="status"
        aria-live="polite"
        style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', gap: SPACE.sm, padding: SPACE.lg }}
      >
        <Spin size="small" />
        <span style={{ fontSize: 12.5, color: TEXT_MUTED, fontFamily: 'var(--font-ui)' }}>{label}</span>
      </div>
    );
  }
  const n = rows ?? (variant === 'form' ? 4 : variant === 'card' ? 3 : 5);
  let body: ReactNode;
  if (variant === 'card') {
    body = (
      <div style={{ background: BG_CARD, border: `1px solid ${BORDER}`, borderRadius: CARD_RADIUS, padding: CARD_PADDING }}>
        <Skeleton active title={{ width: '40%' }} paragraph={{ rows: n }} />
      </div>
    );
  } else if (variant === 'form') {
    body = (
      <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.md }}>
        {Array.from({ length: n }, (_, i) => (
          <div key={i} style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xs }}>
            <Skeleton.Input active size="small" style={{ width: 120, minWidth: 0, height: 14 }} />
            <Skeleton.Input active block style={{ height: 32 }} />
          </div>
        ))}
      </div>
    );
  } else {
    body = (
      <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.sm, padding: `${SPACE.sm}px 0` }}>
        {Array.from({ length: n }, (_, i) => (
          <Skeleton.Input key={i} active block style={{ height: 20, opacity: 1 - i * 0.12 }} />
        ))}
      </div>
    );
  }
  return (
    <div role="status" aria-busy="true" aria-label={label} style={{ flex: 1, minWidth: 0 }}>
      {body}
    </div>
  );
}

/**
 * Grey text bars where figures will appear: for a tile or a small card
 * whose data is still loading. Same as `<LoadingState variant="card">`
 * without the card border, for a container that is already a card.
 */
export function SkeletonState({ rows = 3, label = 'Loading' }: { rows?: number; label?: string }) {
  return (
    <div role="status" aria-busy="true" aria-label={label} style={{ flex: 1, padding: SPACE.sm }}>
      <Skeleton active title={false} paragraph={{ rows }} />
    </div>
  );
}

/**
 * The empty state: optional muted icon, title, one hint sentence, and at
 * most two actions — `action` (the primary: a `type="primary"` Button) and
 * `secondaryAction` (a default Button or a link). `compact` is for a small
 * container (a dashboard tile, a table body): less padding, smaller type.
 */
export function EmptyState({
  icon,
  title,
  hint,
  action,
  secondaryAction,
  compact = false,
}: {
  icon?: ReactNode;
  title: ReactNode;
  hint?: ReactNode;
  action?: ReactNode;
  secondaryAction?: ReactNode;
  compact?: boolean;
}) {
  const { TEXT_PRIMARY, TEXT_MUTED } = useColors();
  return (
    <div
      data-state="empty"
      style={{
        textAlign: 'center',
        padding: compact ? SPACE.md : `${SPACE.xl}px ${SPACE.lg}px`,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        flex: compact ? 1 : undefined,
      }}
    >
      {icon && (
        <div aria-hidden style={{ fontSize: compact ? 22 : 32, color: TEXT_MUTED, lineHeight: 1, marginBottom: compact ? SPACE.xs : SPACE.sm }}>
          {icon}
        </div>
      )}
      <div
        style={{
          fontFamily: 'var(--font-ui)',
          fontSize: compact ? 13.5 : 15,
          fontWeight: 600,
          color: TEXT_PRIMARY,
          lineHeight: 1.3,
        }}
      >
        {title}
      </div>
      {hint && (
        <div
          style={{
            fontFamily: 'var(--font-ui)',
            fontSize: 12.5,
            color: TEXT_MUTED,
            lineHeight: 1.45,
            marginTop: compact ? SPACE.xxs : SPACE.xs,
            maxWidth: 420,
          }}
        >
          {hint}
        </div>
      )}
      {(action || secondaryAction) && (
        <div style={{ marginTop: compact ? SPACE.sm : SPACE.md, display: 'flex', gap: SPACE.xs, flexWrap: 'wrap', justifyContent: 'center' }}>
          {action}
          {secondaryAction}
        </div>
      )}
    </div>
  );
}

/**
 * The error state: "Could not load <what>", ONE plain sentence (see
 * `plainError`), Retry when `onRetry` is given, and a "Details" toggle with
 * the raw text (status, code, request id) for a support request.
 */
export function ErrorState({
  error,
  what,
  onRetry,
  compact = false,
}: {
  error: unknown;
  /** What failed to load, with its article: "the audit log", "the users". */
  what: string;
  onRetry?: () => unknown;
  compact?: boolean;
}) {
  const { TEXT_PRIMARY, TEXT_MUTED, ACCENT_RED } = useColors();
  const [open, setOpen] = useState(false);
  const plain = plainError(error, `The proxy could not load ${what}. Try again.`);
  return (
    <div
      role="alert"
      data-state="error"
      style={{
        textAlign: 'center',
        padding: compact ? SPACE.md : `${SPACE.xl}px ${SPACE.lg}px`,
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
      }}
    >
      <ExclamationCircleOutlined aria-hidden style={{ fontSize: compact ? 22 : 32, color: ACCENT_RED, marginBottom: compact ? SPACE.xs : SPACE.sm }} />
      <div style={{ fontFamily: 'var(--font-ui)', fontSize: compact ? 13.5 : 15, fontWeight: 600, color: TEXT_PRIMARY, lineHeight: 1.3 }}>
        Could not load {what}
      </div>
      <div style={{ fontFamily: 'var(--font-ui)', fontSize: 12.5, color: TEXT_MUTED, lineHeight: 1.45, marginTop: SPACE.xs, maxWidth: 460 }}>
        {plain.text}
      </div>
      <div style={{ marginTop: compact ? SPACE.sm : SPACE.md, display: 'flex', gap: SPACE.xs, flexWrap: 'wrap', justifyContent: 'center' }}>
        {onRetry && (
          <Button icon={<ReloadOutlined />} onClick={() => void onRetry()}>
            Retry
          </Button>
        )}
        {plain.details && (
          <Button type="text" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
            {open ? 'Hide details' : 'Details'}
          </Button>
        )}
      </div>
      {open && plain.details && (
        <code
          style={{
            display: 'block',
            marginTop: SPACE.xs,
            maxWidth: 560,
            fontSize: 11,
            color: TEXT_MUTED,
            overflowWrap: 'anywhere',
            whiteSpace: 'pre-wrap',
            textAlign: 'left',
          }}
        >
          {plain.details}
        </code>
      )}
    </div>
  );
}

type QueryStateProps<T> = {
  /** What the view loads, with its article: the error title is "Could not load <what>". */
  what: string;
  /** Rendered while loading; `<LoadingState variant="list" />` by default. */
  loading?: ReactNode;
  /** Rendered when `isEmpty(data)`; with no `empty`, children render. */
  empty?: ReactNode;
  isEmpty?: (data: T) => boolean;
  compact?: boolean;
  children: (data: T) => ReactNode;
};

/**
 * Renders exactly ONE of loading / error / empty / data for one query
 * (`query`) or several (`queries`, data as a tuple). Retry refetches every
 * query. See the header of this file.
 */
export function QueryState<T>(props: QueryStateProps<T> & { query: QueryLike<T> }): ReactNode;
export function QueryState<T extends readonly unknown[]>(
  props: QueryStateProps<T> & { queries: { [K in keyof T]: QueryLike<T[K]> } },
): ReactNode;
export function QueryState<T>(
  props: QueryStateProps<T> & { query?: QueryLike<T>; queries?: readonly QueryLike<unknown>[] },
): ReactNode {
  const { what, loading, empty, isEmpty, compact, children } = props;
  const all: readonly QueryLike<unknown>[] = props.queries ?? (props.query ? [props.query] : []);
  const view = props.queries
    ? queriesView(props.queries as readonly QueryLike<unknown>[], isEmpty as ((d: readonly unknown[]) => boolean) | undefined)
    : queryView(props.query as QueryLike<T>, isEmpty);
  switch (view.kind) {
    case 'loading':
      return loading ?? <LoadingState variant="list" label={`Loading ${what}`} />;
    case 'error':
      return (
        <ErrorState
          error={view.error}
          what={what}
          compact={compact}
          onRetry={all.some((q) => q.refetch) ? () => all.forEach((q) => q.refetch?.()) : undefined}
        />
      );
    case 'empty':
      return empty ?? children(view.data as T);
    default:
      return children(view.data as T);
  }
}
