/**
 * Jobs shots: the Replication and Lifecycle pages (the rules) and the Jobs
 * page (every run) (docs: how-to/replicate-a-bucket,
 * how-to/expire-and-archive-objects, how-to/move-a-bucket-between-backends,
 * explanation/jobs-and-durability).
 */
import type { Page } from '@playwright/test';
import type { Shot, Target } from '../shot';
import { nav } from './common';
import { at, BUCKET_ENCRYPTION_SHOTS, ENCRYPTION_SHOTS, fakeJson, patchJson } from './encryption';

type Json = Record<string, unknown>;
const JOBS = '/_/api/admin/jobs';
const RUNS = '/_/api/admin/jobs/runs';
const REPLICATION = '/_/admin/storage/replication';
const LIFECYCLE = '/_/admin/storage/lifecycle';
const STORAGE = '/_/api/admin/config/section/storage';

/** The open job drawer. */
const drawer: Target = { css: '.ant-drawer-body' };
const inDrawer = (page: Page) => page.locator('.ant-drawer-body');

async function openNewJobMenu(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'New job' }).click();
  await page.getByRole('menuitem', { name: /^Replication rule/ }).waitFor();
}

async function newJob(page: Page, item: RegExp): Promise<void> {
  await openNewJobMenu(page);
  await page.getByRole('menuitem', { name: item }).click();
}

/** The "⋯" menu of a rule row on the Replication or Lifecycle page. */
async function openRuleMenu(page: Page, rule: string): Promise<void> {
  await page.getByRole('button', { name: `More actions for ${rule}` }).click();
  await page.getByRole('menuitem', { name: 'Clone' }).waitFor();
}

/** No focus ring, no open suggestion list, no hover chip. (No Escape: it closes the drawer.) */
async function calm(page: Page): Promise<void> {
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  // scrollIntoView on a field in the drawer also scrolls the page behind it,
  // by a different amount on each run; the page behind a drawer stays at the top.
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.mouse.move(1, 1);
}

/** Type into an autocomplete and close its suggestion list. */
async function typeIn(page: Page, placeholder: string, value: string): Promise<void> {
  const box = inDrawer(page).getByPlaceholder(placeholder);
  await box.fill(value);
  await box.evaluate((el) => (el as HTMLElement).blur());
}

/** A new lifecycle draft, filled in as expire-nightly-dumps on db-archive/nightly/, 90 days, still disabled. */
async function lifecycleDraft(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'New lifecycle rule' }).click();
  await inDrawer(page).locator('input').first().fill('expire-nightly-dumps');
  await typeIn(page, 'prod-artifacts', 'db-archive');
  await typeIn(page, 'builds/releases/', 'nightly/');
  await inDrawer(page).getByPlaceholder('30d').fill('90d');
  await calm(page);
}

/** The lifecycle rule of the how-to, as the saved config would hold it. */
const NIGHTLY_RULE = {
  name: 'expire-nightly-dumps',
  enabled: true,
  bucket: 'db-archive',
  prefix: 'nightly/',
  action: 'delete',
  expire_after: '90d',
  include_globs: ['nightly/**/*.dump'],
  exclude_globs: ['.deltaglider/**', 'nightly/golden/**'],
  batch_size: 100,
};
const MB = 1024 * 1024;
const DUMPS = ['2026-06-01', '2026-06-02', '2026-06-03'];

/**
 * The server state after the how-to's apply, without changing the proxy
 * (later shots of the run must not see a new rule): GET /jobs and GET storage
 * gain the rule, and its preview lists three old dumps.
 */
async function nightlyRuleSaved(page: Page): Promise<void> {
  await patchJson(page, JOBS, 'GET', (body) => {
    const jobs = body.jobs as Json[];
    const lc = jobs.find((j) => j.kind === 'lifecycle');
    if (lc) {
      jobs.push({
        ...lc,
        id: 'lifecycle:expire-nightly-dumps',
        name: 'expire-nightly-dumps',
        scope: { bucket: 'db-archive', prefix: 'nightly/' },
        status: 'idle',
        status_raw: 'idle',
        last_run_at: null,
        last_error: null,
      });
    }
    return body;
  });
  await patchJson(page, STORAGE, 'GET', (body) => {
    const lifecycle = body.lifecycle as { rules: Json[] };
    lifecycle.rules.push(NIGHTLY_RULE);
    return body;
  });
  await fakeJson(page, `${JOBS}/lifecycle:expire-nightly-dumps/preview`, 'POST', {
    rule_name: 'expire-nightly-dumps',
    status: 'preview',
    objects_scanned: 94,
    objects_affected: DUMPS.length,
    objects_skipped: 0,
    bytes_affected: DUMPS.length * 4 * MB,
    errors: 0,
    candidates: DUMPS.map((d) => ({
      bucket: 'db-archive',
      key: `nightly/${d}.dump`,
      action: 'delete',
      delete_source_after_success: false,
      created_at: `${d}T02:00:00Z`,
      size: 4 * MB,
    })),
    failures: [],
  });
  await page.reload();
  await page.getByText('expire-nightly-dumps').first().waitFor();
}

/** Add one-off runs at the top of GET /jobs/runs (the Jobs page), newest first; nothing runs. */
async function withOneOffRuns(page: Page, runs: Json[], wait: string): Promise<void> {
  await patchJson(page, RUNS, 'GET', (body) => {
    (body.runs as Json[]).unshift(...runs);
    return body;
  });
  await page.reload();
  await page.getByText(wait).first().waitFor();
}

/** A bulk delete of downloads/public/2025/ that dana started in the file browser an hour ago. */
const DANA_DELETE: Json = {
  id: 'maintenance:2',
  job_id: 'maintenance:2',
  kind: 'delete',
  name: 'downloads',
  origin: 'file-browser',
  started_by: 'dana',
  scope: { bucket: 'downloads', prefix: 'public/2025/' },
  trigger: 'oneoff',
  status: 'succeeded',
  status_raw: 'succeeded',
  progress: { processed: 412, total: 412, bytes: 0, failed: 0, skipped: 0 },
  created_at: at(3600),
  started_at: at(3600),
  finished_at: at(3540),
  last_error: null,
  detail: { prefixes: ['public/2025/'], objects: 0 },
};

/** A migrate job of db-archive to hetzner-fsn1 in its copy phase (GET /jobs/runs only; nothing runs). */
async function migrateRunning(page: Page): Promise<void> {
  await withOneOffRuns(
    page,
    [
      {
        id: 'maintenance:1',
        job_id: 'maintenance:1',
        origin: 'admin',
        started_by: 'admin',
        kind: 'migrate',
        name: 'db-archive',
        scope: { bucket: 'db-archive', target: 'hetzner-fsn1' },
        trigger: 'oneoff',
        status: 'running',
        status_raw: 'running',
        phase: 'copy',
        percent: 67,
        progress: { processed: 2, total: 3, bytes: 8 * MB, failed: 0, skipped: 0 },
        created_at: at(90),
        started_at: at(88),
        finished_at: null,
        last_error: null,
        detail: { target_backend: 'hetzner-fsn1', from_backend: 'local-disk', delete_source: false, target: 'fresh' },
      },
    ],
    'db-archive',
  );
}

async function openBucket(page: Page, name: string): Promise<void> {
  // The row's name is its visible text: the bucket name, then the chips.
  const row = new RegExp(`^${name} `);
  await page.getByRole('button', { name: row, expanded: false }).click();
  await page.getByRole('button', { name: row, expanded: true }).waitFor();
}

async function migrateModal(page: Page): Promise<void> {
  await openBucket(page, 'db-archive');
  await page.getByRole('button', { name: 'Migrate data…' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByRole('combobox').click();
  await page.locator('.ant-select-dropdown:visible .ant-select-item-option').filter({ hasText: 'hetzner-fsn1' }).click();
  await calm(page);
}

export const JOBS_SHOTS: Shot[] = [
  {
    id: 'jobs-list',
    route: '/_/admin/jobs',
    alt: 'The Jobs page lists a run of the releases-to-dr replication rule and a bulk delete that dana started in the file browser, newest first; callout 1 marks Jobs in the sidebar, and callout 2 marks New job.',
    setup: (page) => withOneOffRuns(page, [DANA_DELETE], 'File browser · dana'),
    annotations: [
      { target: nav('Jobs'), kind: 'callout', label: '1', side: 'right' },
      { target: { role: 'button', name: 'New job' }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
  {
    id: 'job-runs',
    clip: { union: [{ role: 'button', name: 'New job' }, { text: 'Show runs that changed nothing', exact: true }, { role: 'table' }] },
    clipPadding: 24,
    route: '/_/admin/jobs?rule=replication:releases-to-dr',
    alt: 'The Jobs page shows only the runs of the releases-to-dr rule; the box marks the filter chip of the rule, and the arrow points at the five objects that its run copied.',
    annotations: [
      { target: { css: '.ant-tag:has-text("Replication rule")' }, kind: 'box' },
      { target: { text: /5 copied/ }, kind: 'arrow', side: 'right' },
    ],
  },
  // ── how-to/replicate-a-bucket ──
  {
    id: 'replicate-new-rule',
    route: REPLICATION,
    alt: 'The Replication page; callout 1 marks Replication in the sidebar, and callout 2 marks New replication rule.',
    annotations: [
      { target: nav('Replication'), kind: 'callout', label: '1', side: 'right' },
      { target: { role: 'button', name: 'New replication rule' }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
  {
    id: 'replicate-rule-fields',
    route: `${REPLICATION}?job=replication:releases-to-dr&tab=definition`,
    alt: 'The Definition tab of the releases-to-dr replication rule; callout 3 marks Rule name, callout 4 marks Enabled, callout 5 marks Source, set to releases, and callout 6 marks Destination, set to releases-dr.',
    annotations: [
      { target: { text: 'Rule name', exact: true, within: drawer }, kind: 'callout', label: '3', side: 'left' },
      { target: { text: 'Enabled', exact: true, within: drawer }, kind: 'callout', label: '4', side: 'left' },
      { target: { text: 'Source', exact: true, within: drawer }, kind: 'callout', label: '5', side: 'left' },
      { target: { text: 'Destination', exact: true, within: drawer }, kind: 'callout', label: '6', side: 'left' },
    ],
  },
  {
    id: 'replicate-rule-advanced',
    route: `${REPLICATION}?job=replication:releases-to-dr&tab=definition`,
    alt: 'The Advanced part of the releases-to-dr rule is open; one box marks Batch size, and the other box marks the Conflict policy choices.',
    setup: async (page) => {
      await inDrawer(page).locator('summary', { hasText: /^Advanced/ }).click();
      await inDrawer(page).getByText('Newer wins — safest default').scrollIntoViewIfNeeded();
      await calm(page);
    },
    annotations: [
      { target: { union: [{ text: 'Batch size', exact: true, within: drawer }, { text: /^Objects per listing page/, within: drawer }] }, kind: 'box' },
      { target: { union: [{ text: 'Conflict policy', exact: true, within: drawer }, { text: 'Skip existing destination objects', within: drawer }, { text: /^What the rule does when/, within: drawer }] }, kind: 'box' },
    ],
  },
  {
    id: 'replicate-apply',
    clip: { css: '.ant-modal-container' },
    clipPadding: 32,
    route: `${REPLICATION}?job=replication:releases-to-dr&tab=definition`,
    alt: 'The review dialog lists the changed fields of the releases-to-dr rule and the Replication plan; the arrow points at Apply and Persist.',
    setup: async (page) => {
      // The rule exists in the seed, so the diff shows the one field that the how-to sets differently.
      await inDrawer(page).locator('.dg-field:has(:text-is("Interval")) input').fill('1h');
      await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
      await page.getByTestId('apply-dialog-confirm').waitFor();
    },
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'replicate-run-now',
    clip: { union: [{ role: 'button', name: 'New replication rule' }, { role: 'table' }] },
    clipPadding: 24,
    route: REPLICATION,
    alt: 'The Replication page lists the releases-to-dr rule; the arrow points at its Run now button.',
    annotations: [{ target: { role: 'button', name: 'Run now: releases-to-dr' }, kind: 'arrow', side: 'top' }],
  },
  {
    id: 'replicate-verify',
    route: `${REPLICATION}?job=replication:releases-to-dr&tab=verify`,
    alt: 'The Verify tab of the releases-to-dr rule explains the metadata audit; the arrow points at the Run audit button.',
    annotations: [{ target: { role: 'button', name: 'Run audit' }, kind: 'arrow', side: 'right' }],
  },
  {
    id: 'replicate-clone-menu',
    clip: { union: [{ role: 'button', name: 'New replication rule' }, { role: 'table' }, { role: 'menu' }] },
    clipPadding: 24,
    route: REPLICATION,
    alt: 'The Replication page with the "⋯" menu of the releases-to-dr rule open; callout 1 marks the "⋯" button, and callout 2 marks Clone.',
    setup: (page) => openRuleMenu(page, 'releases-to-dr'),
    annotations: [
      { target: { role: 'button', name: 'More actions for releases-to-dr' }, kind: 'callout', label: '1', side: 'top' },
      { target: { role: 'menuitem', name: 'Clone' }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
  {
    id: 'replicate-clone-draft',
    clip: { css: '.ant-drawer-content-wrapper' },
    // Room for the callouts left of the drawer.
    clipPadding: 48,
    route: REPLICATION,
    alt: 'The drawer of the new draft releases-to-dr-copy; the box marks the warning that the rule releases-to-dr has the same source, destination and filters, callout 3 marks Rule name, callout 4 marks Source, and callout 5 marks Destination.',
    setup: async (page) => {
      await openRuleMenu(page, 'releases-to-dr');
      await page.getByRole('menuitem', { name: 'Clone' }).click();
      await inDrawer(page).getByText('Rule "releases-to-dr" has the same source, destination and filters').waitFor();
      await calm(page);
    },
    annotations: [
      { target: { css: '.ant-drawer-body .ant-alert-warning:has-text("has the same source")' }, kind: 'box' },
      { target: { text: 'Rule name', exact: true, within: drawer }, kind: 'callout', label: '3', side: 'left' },
      { target: { text: 'Source', exact: true, within: drawer }, kind: 'callout', label: '4', side: 'left' },
      { target: { text: 'Destination', exact: true, within: drawer }, kind: 'callout', label: '5', side: 'left' },
    ],
  },
  // ── how-to/expire-and-archive-objects ──
  {
    id: 'lifecycle-new-rule',
    route: LIFECYCLE,
    alt: 'The Lifecycle page; callout 1 marks Lifecycle in the sidebar, and callout 2 marks New lifecycle rule.',
    annotations: [
      { target: nav('Lifecycle'), kind: 'callout', label: '1', side: 'right' },
      { target: { role: 'button', name: 'New lifecycle rule' }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
  {
    id: 'lifecycle-rule-fields',
    route: LIFECYCLE,
    alt: 'A new lifecycle rule named expire-nightly-dumps; callout 3 marks Rule name, callout 4 marks the Enabled switch, which stays off, callout 5 marks Scope, set to db-archive and nightly/, callout 6 marks Expire after, set to 90d, and callout 7 marks Action, set to Delete.',
    setup: lifecycleDraft,
    // The Action field sits below the fold of the default 800px viewport.
    viewport: { width: 1040, height: 960 },
    annotations: [
      { target: { text: 'Rule name', exact: true, within: drawer }, kind: 'callout', label: '3', side: 'left' },
      { target: { text: 'Enabled', exact: true, within: drawer }, kind: 'callout', label: '4', side: 'left' },
      { target: { text: 'Scope', exact: true, within: drawer }, kind: 'callout', label: '5', side: 'left' },
      { target: { text: 'Expire after', exact: true, within: drawer }, kind: 'callout', label: '6', side: 'left' },
      { target: { text: 'Action', exact: true, within: drawer }, kind: 'callout', label: '7', side: 'left' },
    ],
  },
  {
    id: 'lifecycle-rule-filters',
    route: LIFECYCLE,
    alt: 'The Which objects section of the expire-nightly-dumps rule; the box marks Include and Exclude, which hold nightly/**/*.dump and nightly/golden/**.',
    setup: async (page) => {
      await lifecycleDraft(page);
      const which = inDrawer(page).getByRole('region', { name: 'Which objects' });
      await which.getByPlaceholder('*.zip').fill('nightly/**/*.dump');
      await which.getByPlaceholder('*.tmp').fill('.deltaglider/**\nnightly/golden/**');
      // Above the floating dirty bar.
      await which.evaluate((el) => el.scrollIntoView({ block: 'center' }));
      await calm(page);
    },
    annotations: [
      { target: { union: [{ text: 'Include', exact: true, within: drawer }, { placeholder: '*.tmp', within: drawer }] }, kind: 'box' },
    ],
  },
  {
    id: 'lifecycle-transition',
    route: LIFECYCLE,
    alt: 'The expire-nightly-dumps rule with Action set to Archive / move; callout 1 marks Action, callout 2 marks Destination, set to db-archive and cold/nightly/, and callout 3 marks Delete source after copy, which stays off.',
    setup: async (page) => {
      await lifecycleDraft(page);
      await inDrawer(page).locator('.dg-field:has(:text-is("Action")) .ant-select').click();
      await page.locator('.ant-select-dropdown:visible .ant-select-item-option').filter({ hasText: 'Archive / move' }).click();
      await typeIn(page, 'archive-artifacts', 'db-archive');
      const prefix = inDrawer(page).getByPlaceholder('archive/releases/');
      await prefix.fill('cold/nightly/');
      await prefix.evaluate((el) => (el as HTMLElement).blur());
      // Above the floating dirty bar.
      await prefix.evaluate((el) => el.scrollIntoView({ block: 'center' }));
      await calm(page);
    },
    annotations: [
      { target: { text: 'Action', exact: true, within: drawer }, kind: 'callout', label: '1', side: 'left' },
      { target: { text: 'Destination', exact: true, within: drawer }, kind: 'callout', label: '2', side: 'left' },
      { target: { text: 'Delete source after copy', exact: true, within: drawer }, kind: 'callout', label: '3', side: 'left' },
    ],
  },
  {
    id: 'lifecycle-apply',
    clip: { css: '.ant-modal-container' },
    clipPadding: 32,
    route: LIFECYCLE,
    alt: 'The review dialog lists the new expire-nightly-dumps lifecycle rule; the arrow points at Apply and Persist.',
    setup: async (page) => {
      await lifecycleDraft(page);
      await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
      await page.getByTestId('apply-dialog-confirm').waitFor();
      await page.evaluate(() => window.scrollTo(0, 0));
    },
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'arrow', side: 'bottom' }],
  },
  {
    id: 'lifecycle-preview',
    route: `${LIFECYCLE}?job=lifecycle:expire-nightly-dumps&tab=preview`,
    alt: 'The Preview tab of the expire-nightly-dumps rule lists three dumps that the rule would delete, with their total size; the arrow points at Refresh preview.',
    setup: async (page) => {
      await nightlyRuleSaved(page);
      await inDrawer(page).getByText('nightly/2026-06-01.dump').waitFor();
    },
    annotations: [{ target: { role: 'button', name: /Refresh preview/ }, kind: 'arrow', side: 'top' }],
  },
  {
    id: 'lifecycle-scheduler-switch',
    route: LIFECYCLE,
    alt: 'The Lifecycle page with the switch Run lifecycle rules on schedule, which is on; the arrow points at the switch.',
    annotations: [{ target: { role: 'switch', name: 'Run lifecycle rules on schedule' }, kind: 'arrow', side: 'left' }],
  },
  {
    id: 'lifecycle-run-confirm',
    clip: { css: '.ant-modal-container' },
    clipPadding: 32,
    route: LIFECYCLE,
    alt: 'The dialog Run lifecycle rule "expire-nightly-dumps" now? lists the three dumps that the run would delete; the arrow points at the button Run: delete 3 objects.',
    setup: async (page) => {
      await nightlyRuleSaved(page);
      await page.getByRole('button', { name: 'Run now: expire-nightly-dumps' }).click();
      await page.getByRole('button', { name: 'Run: delete 3 objects' }).waitFor();
    },
    annotations: [{ target: { role: 'button', name: 'Run: delete 3 objects' }, kind: 'arrow', side: 'bottom' }],
  },
  // ── how-to/move-a-bucket-between-backends ──
  {
    id: 'migrate-bucket-link',
    route: '/_/admin/storage/buckets',
    alt: 'The Buckets page with the db-archive row open; callout 1 marks Buckets in the sidebar, callout 2 marks the db-archive row, and callout 3 marks the Migrate data… link next to Backend.',
    setup: (page) => openBucket(page, 'db-archive'),
    annotations: [
      { target: nav('Buckets'), kind: 'callout', label: '1', side: 'right' },
      { target: { role: 'button', name: /^db-archive / }, kind: 'callout', label: '2', side: 'left' },
      { target: { role: 'button', name: 'Migrate data…' }, kind: 'box', label: '3' },
    ],
  },
  {
    id: 'migrate-modal',
    route: '/_/admin/storage/buckets',
    alt: 'The dialog Migrate db-archive to another backend; callout 4 marks Target backend, set to hetzner-fsn1, callout 5 marks the Delete source objects after the switch-over checkbox, which stays off, and callout 6 marks Start migration.',
    setup: migrateModal,
    annotations: [
      { target: { text: 'Target backend', exact: true }, kind: 'callout', label: '4', side: 'left' },
      { target: { role: 'checkbox', name: /^Delete source objects after the switch-over/ }, kind: 'callout', label: '5', side: 'left' },
      { target: { role: 'button', name: 'Start migration' }, kind: 'callout', label: '6', side: 'bottom' },
    ],
  },
  {
    id: 'migrate-jobs-row',
    clip: { union: [{ role: 'button', name: 'New job' }, { text: 'Show runs that changed nothing', exact: true }, { role: 'table' }] },
    clipPadding: 24,
    route: '/_/admin/jobs',
    alt: 'The Jobs page lists the running migrate job of db-archive to hetzner-fsn1; the box marks the job row.',
    setup: migrateRunning,
    annotations: [{ target: { css: '[role="row"]:has(:text("Migrate"))' }, kind: 'box' }],
  },
  // ── how-to/migrate-existing-data-into-the-proxy (batch A) ──
  {
    id: 'adopt-bucket-backfill',
    route: '/_/admin/jobs',
    alt: 'The dialog Backfill object metadata with the releases bucket selected; the arrow points at Start now.',
    setup: async (page) => {
      await newJob(page, /^Backfill metadata/);
      await page.getByRole('checkbox', { name: 'releases', exact: true }).check();
      // check() may scroll the page behind the dialog; put it back.
      await page.evaluate(() => window.scrollTo(0, 0));
      await calm(page);
    },
    annotations: [{ target: { role: 'button', name: /^Start now/ }, kind: 'arrow', side: 'bottom' }],
  },
  ...ENCRYPTION_SHOTS,
  ...BUCKET_ENCRYPTION_SHOTS,
];
