/**
 * Jobs — every run, live and past, on one list: the runs of replication and
 * lifecycle rules, and the one-off jobs (bulk delete, re-encrypt, migrate,
 * metadata backfill; a one-off is a run without a rule). The rules
 * themselves are on the Replication and Lifecycle pages (RulesPanel).
 *
 * Reads `GET /jobs/runs` page by page; it polls fast while a run in it is
 * live. The filters and the open drawer live in the URL (?status, ?kind,
 * ?bucket, ?rule, ?noop, ?job).
 */
import { useEffect, useMemo, useState } from 'react';
import { Alert, Button, Segmented, Select, Space, Spin, Switch, Tag, Typography, message } from 'antd';
import { CloseOutlined, DeleteOutlined, PlusOutlined, StopOutlined, ThunderboltOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import Dropdown from '../KeyboardDropdown';
import type { RunEntry, RunFilters } from '../../adminApi';
import { runJobAction } from '../../adminApi';
import {
  cancelConfirmText,
  isActiveJobStatus,
  jobStatusLabel,
  jobStatusTone,
  kindLabel,
  kindTone,
  parseJobId,
  rowMeterInput,
  runDuration,
  scopeText,
} from '../../jobsView';
import {
  isRuleRun,
  runActions,
  runFiltersFromQuery,
  runFiltersToQuery,
  runName,
  runOriginLabel,
  runSubject,
  RUN_KIND_OPTIONS,
  type RunAction,
} from '../../runsView';
import { useRunList } from '../../queries/jobs';
import { useBucketNames } from '../../queries/backends';
import { qk } from '../../queries/keys';
import { useNavigation } from '../../NavigationContext';
import { buildViewUrl, parseAdminQuery } from '../../urlState';
import { useOverlayClose } from '../../hooks/useOverlayClose';
import { useNow } from '../../hooks/useNow';
import { normalizeUiError } from '../../errorHandling';
import { useCardStyles, contentColumn, CONTENT_WIDE } from '../shared-styles';
import TimeAgo from '../TimeAgo';
import RowActionsMenu from '../RowActionsMenu';
import ReencryptProposalModal from '../ReencryptProposalModal';
import BackfillMetadataModal from '../BackfillMetadataModal';
import MigrateBucketModal from '../MigrateBucketModal';
import RecordList, { type RecordColumn } from './RecordList';
import OutcomeMeter from './OutcomeMeter';
import JobDrawer from './JobDrawer';
import RunDrawer from './RunDrawer';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
  /** Raw query string (with leading `?`): the filters and the open drawer. */
  search?: string;
}

/** The confirmation of each run action. */
function actionConfirm(run: RunEntry, action: RunAction): { title: string; content: string; okText: string } {
  switch (action) {
    case 'cancel':
      return { ...cancelConfirmText(run), okText: 'Cancel job' };
    case 'kill':
      return {
        title: `Kill the running "${runName(run)}" run?`,
        content: 'Transfers in progress stop immediately.',
        okText: 'Kill run',
      };
    case 'remove':
      return {
        title: `Remove this ${kindLabel(run.kind).toLowerCase()} job from the list?`,
        content: 'This removes the job record and its failures. It changes no object.',
        okText: 'Remove',
      };
  }
}

const ACTION_UI: Record<RunAction, { label: string; icon: React.ReactNode; wire: 'cancel' | 'kill' | 'delete'; done: string }> = {
  cancel: {
    label: 'Cancel',
    icon: <StopOutlined />,
    wire: 'cancel',
    done: 'Cancellation requested — the job stops at the next safe point',
  },
  kill: { label: 'Kill run', icon: <ThunderboltOutlined />, wire: 'kill', done: 'Kill requested' },
  remove: { label: 'Remove', icon: <DeleteOutlined />, wire: 'delete', done: 'Job removed from the list' },
};

/** When a run started, and how long it ran (or runs). */
function WhenCell({ run }: { run: RunEntry }) {
  const live = isActiveJobStatus(run.status);
  const nowSec = useNow(live ? 1000 : 60_000) / 1000;
  const took = runDuration(run.started_at ?? run.created_at, run.finished_at, nowSec);
  return (
    <div style={{ fontSize: 12, whiteSpace: 'nowrap', lineHeight: 1.4 }}>
      <Text style={{ fontSize: 12, display: 'block' }}>
        <TimeAgo ts={run.started_at ?? run.created_at} />
      </Text>
      {took && (
        <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>
          {live ? `running ${took}` : `took ${took}`}
        </Text>
      )}
    </div>
  );
}

export default function JobsPanel({ search }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const qc = useQueryClient();
  const [messageApi, msgCtx] = message.useMessage();
  const { navigate } = useNavigation();
  const { markPushed, closeOverlay } = useOverlayClose();
  const bucketNames = useBucketNames();

  // The filters and the open drawer: local state, mirrored to the URL (a
  // shared link, Back/Forward and a reload land on the same view).
  const query = useMemo(() => parseAdminQuery(search ?? ''), [search]);
  const [filters, setFiltersState] = useState<RunFilters>(() => runFiltersFromQuery(query));
  const [openJob, setOpenJob] = useState<string | null>(query.job ?? null);
  const [tab, setTab] = useState<string>(query.tab ?? 'definition');
  useEffect(() => {
    setFiltersState(runFiltersFromQuery(query));
    setOpenJob(query.job ?? null);
    setTab(query.tab ?? 'definition');
  }, [query]);

  // A link to a RULE (an older `jobs?job=replication:x` link) opens the rule
  // page: the rule's definition lives there now.
  useEffect(() => {
    if (!openJob || openJob.includes('#')) return;
    const kind = parseJobId(openJob)?.subsystem;
    if (kind === 'replication' || kind === 'lifecycle') {
      const tab = query.tab;
      navigate(buildViewUrl('admin', `storage/${kind}`, tab ? { job: openJob, tab } : { job: openJob }), {
        replace: true,
      });
    }
  }, [openJob, query.tab, navigate]);

  const setUrl = (f: RunFilters, job: string | null, push = false, nextTab?: string) => {
    setFiltersState(f);
    setOpenJob(job);
    setTab(nextTab ?? 'definition');
    const q = runFiltersToQuery(f, job);
    if (job && nextTab && nextTab !== 'definition') q.tab = nextTab;
    navigate(buildViewUrl('admin', 'jobs', Object.keys(q).length > 0 ? q : undefined), { replace: !push });
    if (push) markPushed();
  };
  const setFilters = (patch: Partial<RunFilters>) => setUrl({ ...filters, ...patch }, null);

  const list = useRunList(filters);
  const runs: RunEntry[] = useMemo(() => (list.data?.pages ?? []).flatMap((p) => p.runs), [list.data]);
  const openRun = openJob ? (runs.find((r) => r.id === openJob) ?? null) : null;

  const [reencryptOpen, setReencryptOpen] = useState(false);
  const [backfillOpen, setBackfillOpen] = useState(false);
  const [migrateOpen, setMigrateOpen] = useState(false);
  const [newMenuOpen, setNewMenuOpen] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);

  const refresh = (run: RunEntry) => {
    qc.invalidateQueries({ queryKey: qk.jobs.runList() });
    qc.invalidateQueries({ queryKey: qk.jobs.list() });
    qc.invalidateQueries({ queryKey: qk.jobs.one(run.job_id) });
  };

  const runAction = async (run: RunEntry, action: RunAction) => {
    setBusy(`${run.id}:${action}`);
    try {
      await runJobAction(run.job_id, ACTION_UI[action].wire);
      messageApi.success(ACTION_UI[action].done);
      if (action === 'remove' && openJob === run.id) closeDrawer();
      refresh(run);
    } catch (e) {
      messageApi.error(normalizeUiError(e, `${ACTION_UI[action].label} failed`));
    } finally {
      setBusy(null);
    }
  };

  const openDrawer = (run: RunEntry, tab?: string) => setUrl(filters, run.id, true, tab);
  const closeDrawer = () => {
    setOpenJob(null);
    setTab('definition');
    closeOverlay(buildViewUrl('admin', 'jobs', runFiltersToQuery(filters)), navigate);
  };

  const newMenu = {
    items: [
      { key: 'reencrypt', label: 'Re-encrypt buckets… — one-off rewrite' },
      { key: 'migrate', label: 'Migrate bucket… — one-off move' },
      { key: 'backfill', label: 'Backfill metadata… — for objects written without the proxy' },
      { type: 'divider' as const },
      { key: 'replication', label: 'Replication rule… — on the Replication page' },
      { key: 'lifecycle', label: 'Lifecycle rule… — on the Lifecycle page' },
    ],
    onClick: ({ key }: { key: string }) => {
      setNewMenuOpen(false);
      if (key === 'reencrypt') setReencryptOpen(true);
      else if (key === 'migrate') setMigrateOpen(true);
      else if (key === 'backfill') setBackfillOpen(true);
      else navigate(buildViewUrl('admin', `storage/${key}`));
    },
  };

  const columns: RecordColumn<RunEntry>[] = [
    {
      key: 'job',
      label: 'Job',
      hideLabelOnNarrow: true,
      track: 'minmax(200px,2fr)',
      render: (r) => {
        const subject = runSubject(r);
        return (
          <div style={{ minWidth: 0 }}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
              <Tag color={kindTone(r.kind)} style={{ margin: 0, flexShrink: 0 }}>
                {kindLabel(r.kind)}
              </Tag>
              <Text
                strong
                ellipsis
                title={runName(r)}
                style={{ fontFamily: 'var(--font-mono)', fontSize: 13, minWidth: 0 }}
              >
                {runName(r)}
              </Text>
            </div>
            {subject && (
              <Text type="secondary" className="dg-cell-truncate" title={subject} style={{ display: 'block', fontSize: 12 }}>
                {subject}
              </Text>
            )}
          </div>
        );
      },
    },
    {
      key: 'origin',
      label: 'Started by',
      narrowHalf: true,
      track: 'max-content',
      render: (r) => (
        <Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
          {runOriginLabel(r)}
        </Text>
      ),
    },
    {
      key: 'scope',
      label: 'Scope',
      track: 'minmax(140px,1.2fr)',
      render: (r) => {
        const scope = scopeText(r.scope);
        return (
          <Text type="secondary" className="dg-cell-truncate" title={scope} style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>
            {scope || '—'}
          </Text>
        );
      },
    },
    {
      key: 'status',
      label: 'Status',
      hideLabelOnNarrow: true,
      track: 'minmax(0,1fr)',
      render: (r) => (
        <div>
          <Tag color={jobStatusTone(r)} style={{ margin: '0 0 4px' }}>
            {jobStatusLabel(r)}
          </Tag>
          <OutcomeMeter {...rowMeterInput(r)} />
          {r.progress.failed > 0 && (
            <Button
              type="link"
              size="small"
              danger
              style={{ padding: '0 4px', height: 'auto', fontSize: 12 }}
              aria-label={`${r.progress.failed} failed — show failures: ${runName(r)}`}
              title="Show the failed objects"
              onClick={(e) => {
                // The row click opens the drawer's first tab; this opens Failures.
                e.stopPropagation();
                openDrawer(r, 'failures');
              }}
            >
              {r.progress.failed.toLocaleString()} failed
            </Button>
          )}
          {r.last_error && (
            <Text type="danger" style={{ display: 'block', fontSize: 11, marginTop: 2 }} ellipsis title={r.last_error}>
              {r.last_error}
            </Text>
          )}
        </div>
      ),
    },
    {
      key: 'when',
      label: 'Started',
      narrowHalf: true,
      track: 'max-content',
      render: (r) => <WhenCell run={r} />,
    },
    {
      key: 'actions',
      label: 'Actions',
      track: 'max-content',
      align: 'end',
      hideLabelOnNarrow: true,
      render: (r) => {
        const actions = runActions(r);
        if (actions.length === 0) return null;
        return (
          <Space size={2} onClick={(e) => e.stopPropagation()}>
            <RowActionsMenu
              label={`More actions for ${runName(r)}`}
              loading={actions.some((a) => busy === `${r.id}:${a}`)}
              actions={actions.map((a) => ({
                key: a,
                danger: true as const,
                icon: ACTION_UI[a].icon,
                label: `${ACTION_UI[a].label}…`,
                confirm: actionConfirm(r, a),
                onSelect: () => runAction(r, a),
              }))}
            />
          </Space>
        );
      },
    },
  ];

  const ruleLabel = filters.rule ? parseJobId(filters.rule) : null;
  const filtered = !!(filters.status || filters.kind || filters.bucket || filters.rule);

  return (
    <div style={contentColumn(CONTENT_WIDE)}>
      {msgCtx}
      <ReencryptProposalModal
        open={reencryptOpen}
        transition="encrypt"
        backendName=""
        buckets={[]}
        pickBuckets
        onClose={() => setReencryptOpen(false)}
      />
      <BackfillMetadataModal open={backfillOpen} onClose={() => setBackfillOpen(false)} />
      <MigrateBucketModal open={migrateOpen} bucket={null} onClose={() => setMigrateOpen(false)} />
      {openRun && isRuleRun(openRun) ? (
        <RunDrawer run={openRun} onClose={closeDrawer} />
      ) : (
        <JobDrawer
          jobId={openJob && openJob.startsWith('maintenance:') ? openJob : null}
          rows={runs.filter((r) => !isRuleRun(r))}
          activeTab={tab}
          onTabChange={(t) => setUrl(filters, openJob, false, t)}
          onCancelJob={(row) => {
            const run = runs.find((r) => r.id === row.id);
            if (!run) return;
            return runJobAction(run.job_id, 'cancel')
              .then(() => refresh(run))
              .catch((e) => messageApi.error(normalizeUiError(e, 'Cancel failed')));
          }}
          inputRadius={inputRadius}
          onClose={closeDrawer}
        />
      )}

      <div style={cardStyle}>
        {/* The filters wrap on their own; "New job" stays top right. */}
        <div style={{ display: 'flex', alignItems: 'flex-start', justifyContent: 'space-between', gap: 12 }}>
          <Space wrap size={8} style={{ flex: 1, minWidth: 0 }}>
            <Segmented
              aria-label="Status"
              value={filters.status ?? 'all'}
              options={[
                { value: 'all', label: 'All' },
                { value: 'active', label: 'Active' },
                { value: 'failed', label: 'Failed' },
              ]}
              onChange={(v) => setFilters({ status: v === 'all' ? undefined : (v as RunFilters['status']) })}
            />
            <Select
              id="runs-filter-kind"
              aria-label="Kind"
              allowClear
              placeholder="Every kind"
              style={{ minWidth: 160 }}
              value={filters.kind}
              options={RUN_KIND_OPTIONS}
              onChange={(v) => setFilters({ kind: v || undefined })}
            />
            <Select
              id="runs-filter-bucket"
              aria-label="Bucket"
              allowClear
              showSearch
              placeholder="Every bucket"
              style={{ minWidth: 160 }}
              value={filters.bucket}
              options={bucketNames.map((b) => ({ value: b, label: b }))}
              onChange={(v) => setFilters({ bucket: v || undefined })}
            />
            {ruleLabel && (
              <Tag
                closable
                closeIcon={<CloseOutlined aria-label="Show every rule" />}
                onClose={() => setFilters({ rule: undefined })}
                style={{ margin: 0, padding: '2px 8px' }}
              >
                {kindLabel(ruleLabel.subsystem)} rule <span style={{ fontFamily: 'var(--font-mono)' }}>{ruleLabel.key}</span>
              </Tag>
            )}
            <label style={{ display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: 12 }}>
              <Switch
                size="small"
                checked={!!filters.noop}
                onChange={(on) => setFilters({ noop: on || undefined })}
                aria-label="Show runs that changed nothing"
              />
              Show runs that changed nothing
            </label>
          </Space>
          <Dropdown menu={newMenu} trigger={['click']} open={newMenuOpen} onOpenChange={setNewMenuOpen}>
            <Button type="primary" icon={<PlusOutlined />} style={{ flexShrink: 0 }}>
              New job
            </Button>
          </Dropdown>
        </div>

        {list.error ? (
          <Alert
            type="error"
            showIcon
            style={{ marginTop: 16, borderRadius: 8 }}
            title={normalizeUiError(list.error, 'Failed to load the runs')}
          />
        ) : list.isLoading ? (
          <div style={{ display: 'flex', justifyContent: 'center', padding: 48 }}>
            <Spin />
          </div>
        ) : (
          <div style={{ marginTop: 16 }}>
            <RecordList
              rows={runs}
              columns={columns}
              rowKey={(r) => r.id}
              onRowClick={openDrawer}
              empty={
                filtered
                  ? 'No run matches these filters.'
                  : 'No runs yet. Rules run on their schedule; a bulk delete in the file browser, or "New job", starts a one-off.'
              }
            />
            {list.hasNextPage && (
              <div style={{ display: 'flex', justifyContent: 'center', marginTop: 12 }}>
                <Button loading={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()}>
                  Show older runs
                </Button>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

