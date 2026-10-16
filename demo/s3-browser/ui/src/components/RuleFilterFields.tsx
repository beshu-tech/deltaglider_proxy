import { useEffect, useMemo, useState } from 'react';
import { Button, Empty, Input, Modal, Segmented, Spin } from 'antd';
import { CheckCircleFilled, CloseCircleFilled, EyeOutlined, MinusCircleFilled } from '@ant-design/icons';
import type { FilterPreview, FilterVerdict } from '../adminApi/jobs';
import { useColors } from '../ThemeContext';
import { useFilterPreview } from '../queries/filterPreview';
import { filterSummary, globReport, scopeLabel, splitAtPrefix, verdictText } from '../ruleFilterPreview';
import FormField from './FormField';
import GlobListTextArea from './GlobListTextArea';

interface Props {
  /** The bucket and prefix the rule reads: the preview samples them. */
  bucket: string;
  prefix: string;
  include: string[];
  exclude: string[];
  onChange: (patch: { include_globs?: string[]; exclude_globs?: string[] }) => void;
  /** `storage.replication.rules[]` or `storage.lifecycle.rules[]`. */
  yamlBase: string;
  /** What an included object does: "replicate", "are candidates". */
  verb: string;
  excludeHelp: string;
  /** A client-side glob error per list (lifecycle validates before apply). */
  includeError?: string | null;
  excludeError?: string | null;
  inputRadius: { borderRadius: number };
}

const SECTION_TITLE: React.CSSProperties = {
  fontSize: 12,
  fontWeight: 700,
  letterSpacing: 0.5,
  textTransform: 'uppercase',
};

/**
 * The include/exclude globs of a rule, and what they select. Under each
 * list, every glob shows how many objects of a sample it matches; a glob
 * that matches nothing because it lacks the prefix offers the prefixed glob.
 * A line under the lists sums the sample up, and Preview opens a wide
 * window with the keys and their verdicts. The preview reads the globs as
 * they are typed; they go into the rule when a field loses focus.
 */
export default function RuleFilterFields(props: Props) {
  const { bucket, prefix, include, exclude, verb } = props;
  const colors = useColors();
  const [draft, setDraft] = useState({ include, exclude });
  const [open, setOpen] = useState(false);
  // A committed change (blur, a suggestion, another editor) re-seeds the draft.
  const committed = JSON.stringify([include, exclude]);
  useEffect(() => {
    setDraft({ include, exclude });
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `committed` is their value
  }, [committed]);

  const preview = useFilterPreview({ bucket, prefix, include: draft.include, exclude: draft.exclude });
  const scope = scopeLabel(bucket, prefix);

  return (
    <section aria-label="Which objects" style={{ marginTop: 20 }}>
      <div style={{ ...SECTION_TITLE, color: colors.TEXT_SECONDARY }}>Which objects</div>
      <p style={{ margin: '6px 0 12px', fontSize: 12, color: colors.TEXT_MUTED, maxWidth: 760 }}>
        Globs match the whole key from the bucket root, so the prefix is part of it. <code>*</code> also
        matches <code>/</code>, and <code>**</code> matches any depth. An exclude wins over an include.
      </p>
      <FilterEditors {...props} draft={draft} setDraft={setDraft} preview={preview.data} rows={4} />
      <div
        role="status"
        aria-label="Filter preview summary"
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 12,
          marginTop: 12,
          padding: '10px 14px',
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 8,
          background: colors.BG_ELEVATED,
        }}
      >
        <div style={{ flex: 1, minWidth: 0, fontSize: 13, color: colors.TEXT_SECONDARY }}>
          {!bucket
            ? 'Choose a bucket to preview what the filters select.'
            : preview.isError
              ? `The preview failed: ${(preview.error as Error).message}`
              : preview.data
                ? filterSummary(preview.data, scope, verb)
                : 'Reading a sample of the bucket…'}
        </div>
        {preview.isFetching && <Spin size="small" aria-label="Updating the preview" />}
        <Button icon={<EyeOutlined />} onClick={() => setOpen(true)} disabled={!bucket}>
          Preview objects
        </Button>
      </div>
      <FilterPreviewModal
        {...props}
        open={open}
        onClose={() => setOpen(false)}
        draft={draft}
        setDraft={setDraft}
        preview={preview.data}
        fetching={preview.isFetching}
        scope={scope}
      />
    </section>
  );
}

type Draft = { include: string[]; exclude: string[] };

/** The two glob lists, each with its per-glob report. */
function FilterEditors({
  include,
  exclude,
  onChange,
  yamlBase,
  excludeHelp,
  includeError,
  excludeError,
  inputRadius,
  draft,
  setDraft,
  preview,
  rows,
}: Props & { draft: Draft; setDraft: (d: Draft) => void; preview: FilterPreview | undefined; rows: number }) {
  const lists = [
    {
      list: 'include' as const,
      label: 'Include',
      help: 'One glob per line. When the list is empty, every object under the prefix is in.',
      value: include,
      draft: draft.include,
      placeholder: '*.zip',
      error: includeError,
      commit: (v: string[]) => onChange({ include_globs: v }),
      setDraft: (v: string[]) => setDraft({ ...draft, include: v }),
    },
    {
      list: 'exclude' as const,
      label: 'Exclude',
      help: excludeHelp,
      value: exclude,
      draft: draft.exclude,
      placeholder: '*.tmp',
      error: excludeError,
      commit: (v: string[]) => onChange({ exclude_globs: v }),
      setDraft: (v: string[]) => setDraft({ ...draft, exclude: v }),
    },
  ];
  return (
    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 16 }}>
      {lists.map((l) => (
        <FormField key={l.list} label={l.label} yamlPath={`${yamlBase}.${l.list}_globs`} helpText={l.help}>
          <GlobListTextArea
            value={l.value}
            onChange={l.commit}
            onDraft={l.setDraft}
            rows={rows}
            placeholder={l.placeholder}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            status={l.error || l.draft.some((g) => globReport(preview, l.list, g)?.error) ? 'error' : undefined}
          />
          {l.error && (
            <div role="alert" style={{ color: 'var(--ant-color-error, #ff4d4f)', fontSize: 12, marginTop: 4 }}>
              {l.error}
            </div>
          )}
          <GlobReports
            list={l.list}
            globs={l.draft}
            preview={preview}
            onUse={(from, to) => {
              const next = l.draft.map((g) => (g === from ? to : g));
              l.setDraft(next);
              l.commit(next);
            }}
          />
        </FormField>
      ))}
    </div>
  );
}

/** One line per glob: how many sampled objects it matches, or why it fails. */
function GlobReports({
  list,
  globs,
  preview,
  onUse,
}: {
  list: 'include' | 'exclude';
  globs: string[];
  preview: FilterPreview | undefined;
  onUse: (from: string, to: string) => void;
}) {
  const colors = useColors();
  if (globs.length === 0 || !preview) return null;
  return (
    <ul aria-label={`${list} glob matches`} style={{ listStyle: 'none', margin: '6px 0 0', padding: 0, fontSize: 12 }}>
      {globs.map((g) => {
        const r = globReport(preview, list, g);
        if (!r) return null;
        return (
          <li key={g} style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: 6, padding: '2px 0' }}>
            <code style={{ color: colors.TEXT_PRIMARY }}>{g}</code>
            {r.error ? (
              <span style={{ color: colors.ACCENT_RED }}>is not a valid glob: {r.error}</span>
            ) : (
              // Amber only when a fix is offered: a safety glob such as
              // `.deltaglider/**` is meant to match nothing.
              <span style={{ color: r.suggestion ? colors.ACCENT_AMBER : colors.TEXT_SECONDARY }}>
                {r.matches > 0
                  ? `matches ${r.matches.toLocaleString('en-US')} of the sample`
                  : 'matches nothing in the sample'}
              </span>
            )}
            {r.suggestion && (
              <Button size="small" type="link" style={{ padding: 0, height: 'auto' }} onClick={() => onUse(g, r.suggestion!)}>
                Use {r.suggestion}
              </Button>
            )}
          </li>
        );
      })}
    </ul>
  );
}

type Tab = 'included' | 'excluded' | 'not_included' | 'all';

/** The wide window: the lists on the left, the sampled keys and their verdicts on the right. */
function FilterPreviewModal({
  open,
  onClose,
  draft,
  setDraft,
  preview,
  fetching,
  scope,
  ...props
}: Props & {
  open: boolean;
  onClose: () => void;
  draft: Draft;
  setDraft: (d: Draft) => void;
  preview: FilterPreview | undefined;
  fetching: boolean;
  scope: string;
}) {
  const colors = useColors();
  const [tab, setTab] = useState<Tab>('included');
  const [search, setSearch] = useState('');
  const [probe, setProbe] = useState('');
  const probed = useFilterPreview({
    bucket: props.bucket,
    prefix: props.prefix,
    include: draft.include,
    exclude: draft.exclude,
    keys: probe.trim() ? [probe.trim()] : [],
    enabled: open,
  });
  const rows = useMemo(() => {
    const keys = preview?.keys ?? [];
    const q = search.trim().toLowerCase();
    return keys.filter(
      (k) =>
        (tab === 'all' || k.verdict === tab || (tab === 'not_included' && k.verdict === 'internal')) &&
        (!q || k.key.toLowerCase().includes(q)),
    );
  }, [preview, tab, search]);
  const n = (v: number | undefined) => (v ?? 0).toLocaleString('en-US');
  const verbTitle = props.verb === 'replicate' ? 'Replicate' : 'In the rule';

  return (
    <Modal
      open={open}
      onCancel={onClose}
      title={
        <div>
          <div>Preview the filters</div>
          <div style={{ fontSize: 12, fontWeight: 400, color: colors.TEXT_SECONDARY, fontFamily: 'var(--font-mono)' }}>
            {scope}
          </div>
        </div>
      }
      width="min(1120px, 96vw)"
      style={{ top: 32 }}
      footer={
        <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
          <span style={{ flex: 1, textAlign: 'left', fontSize: 12, color: colors.TEXT_MUTED }}>
            {preview?.truncated
              ? `A sample: the first ${n(preview.sampled)} objects. The rule acts on every object under the prefix.`
              : 'The globs go into the rule as you leave a field; Review & apply saves the rule.'}
          </span>
          <Button type="primary" onClick={onClose}>
            Done
          </Button>
        </div>
      }
      destroyOnHidden
    >
      <div style={{ display: 'grid', gridTemplateColumns: 'minmax(260px, 340px) 1fr', gap: 24, alignItems: 'start' }}>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 16 }}>
          <div style={{ display: 'grid', gap: 12 }}>
            <FilterEditors {...props} draft={draft} setDraft={setDraft} preview={preview} rows={4} />
          </div>
        </div>
        <div style={{ minWidth: 0 }}>
          <div style={{ marginBottom: 14 }}>
            <label htmlFor="filter-probe" style={{ fontSize: 13, fontWeight: 600 }}>
              Test a key
            </label>
            <Input
              id="filter-probe"
              value={probe}
              onChange={(e) => setProbe(e.target.value)}
              placeholder={`${props.prefix}path/to/object.zip`}
              allowClear
              style={{ fontFamily: 'var(--font-mono)', marginTop: 4 }}
            />
            {/* Room for the verdict, so the list below does not jump. Only the
                verdict of the key in the box: an older answer stays out. */}
            <div style={{ minHeight: 30 }}>
              {probed.data?.keys[0] && probed.data.keys[0].key === probe.trim() && (
                <VerdictLine verdict={probed.data.keys[0].verdict} glob={probed.data.keys[0].glob} verb={props.verb} />
              )}
            </div>
          </div>
          <div style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap', marginBottom: 10 }}>
            <Segmented<Tab>
              value={tab}
              onChange={setTab}
              options={[
                { value: 'included', label: `${verbTitle} · ${n(preview?.included)}` },
                { value: 'excluded', label: `Excluded · ${n(preview?.excluded)}` },
                { value: 'not_included', label: `No include match · ${n((preview?.not_included ?? 0) + (preview?.internal ?? 0))}` },
                { value: 'all', label: `All · ${n(preview?.sampled)}` },
              ]}
            />
            <Input.Search
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              placeholder="Find a key"
              allowClear
              style={{ flex: '1 1 180px', minWidth: 160 }}
            />
            {fetching && <Spin size="small" />}
          </div>
          <div
            role="list"
            aria-label="Sampled objects"
            style={{
              height: 'max(240px, calc(100vh - 420px))',
              overflow: 'auto',
              border: `1px solid ${colors.BORDER}`,
              borderRadius: 8,
            }}
          >
            {rows.length === 0 ? (
              <Empty
                image={Empty.PRESENTED_IMAGE_SIMPLE}
                description={preview ? 'No object in this group.' : 'Reading a sample of the bucket…'}
                style={{ marginTop: 48 }}
              />
            ) : (
              rows.slice(0, 2000).map((k) => {
                const [pre, rest] = splitAtPrefix(k.key, props.prefix);
                return (
                  <div
                    role="listitem"
                    key={k.key}
                    style={{
                      display: 'flex',
                      alignItems: 'center',
                      gap: 12,
                      padding: '6px 12px',
                      borderBottom: `1px solid ${colors.BORDER}`,
                      fontSize: 12,
                    }}
                  >
                    <VerdictIcon verdict={k.verdict} />
                    <span
                      style={{ flex: 1, minWidth: 0, fontFamily: 'var(--font-mono)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
                      title={k.key}
                    >
                      <span style={{ color: colors.TEXT_FAINT }}>{pre}</span>
                      <span style={{ color: colors.TEXT_PRIMARY }}>{rest}</span>
                    </span>
                    {k.glob && (
                      <code style={{ color: colors.TEXT_SECONDARY, whiteSpace: 'nowrap' }}>{k.glob}</code>
                    )}
                  </div>
                );
              })
            )}
          </div>
        </div>
      </div>
    </Modal>
  );
}

function VerdictIcon({ verdict }: { verdict: FilterVerdict }) {
  const colors = useColors();
  if (verdict === 'included') return <CheckCircleFilled aria-label="in" style={{ color: colors.ACCENT_GREEN }} />;
  if (verdict === 'excluded') return <CloseCircleFilled aria-label="excluded" style={{ color: colors.ACCENT_RED }} />;
  return <MinusCircleFilled aria-label="not included" style={{ color: colors.TEXT_FAINT }} />;
}

function VerdictLine({ verdict, glob, verb }: { verdict: FilterVerdict; glob?: string; verb: string }) {
  return (
    <div role="status" aria-label="Key verdict" style={{ display: 'flex', alignItems: 'center', gap: 8, marginTop: 8, fontSize: 13 }}>
      <VerdictIcon verdict={verdict} />
      <span>{verdictText(verdict, glob, verb)}</span>
    </div>
  );
}
