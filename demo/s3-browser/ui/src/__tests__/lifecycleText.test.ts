import { describe, expect, test } from 'vitest';
import {
  CONFIRM_MAX_PREVIEW_AGE_MS,
  confirmNeedsFreshPreview,
  firstRunText,
  globError,
  lifecycleActionCounts,
  lifecycleActionLabel,
  lifecycleRunResultText,
  lifecycleSkipText,
  newestSettledRun,
  parseTickMs,
  previewAgeText,
  ruleGlobError,
} from '../components/jobs/lifecycleText';

describe('lifecycleActionLabel (#4)', () => {
  const t = { action: 'transition', destination_bucket: 'archive', destination_key: 'cold/a.zip' };
  test('a transition says copy or move, with the destination key', () => {
    expect(lifecycleActionLabel({ ...t, delete_source_after_success: false })).toBe('copy to archive/cold/a.zip');
    expect(lifecycleActionLabel({ ...t, delete_source_after_success: true })).toBe('move to archive/cold/a.zip');
  });
  test('a delete stays a delete', () => {
    expect(lifecycleActionLabel({ action: 'delete', delete_source_after_success: false })).toBe('delete');
  });
});

describe('preview split (#9)', () => {
  test('tells not-yet-due from excluded', () => {
    expect(lifecycleSkipText({ objects_not_due: 12, objects_excluded: 1 })).toBe(
      'Of the other scanned objects, 12 are not old enough yet, 1 is kept out by a glob.',
    );
    expect(lifecycleSkipText({})).toBeNull();
  });
  test('counts by action', () => {
    expect(lifecycleActionCounts({ objects_deleted: 46, objects_moved: 0, objects_copied: 3 })).toBe('delete 46, copy 3');
  });
});

describe('confirm freshness (#1)', () => {
  const now = Date.parse('2026-10-13T10:00:30Z');
  test('a missing or old preview asks for a new walk', () => {
    expect(confirmNeedsFreshPreview(undefined, now)).toBe(true);
    expect(confirmNeedsFreshPreview('2026-10-13T10:00:00Z', now)).toBe(true);
    expect(confirmNeedsFreshPreview(new Date(now - CONFIRM_MAX_PREVIEW_AGE_MS + 1000).toISOString(), now)).toBe(false);
  });
  test('says how old the walk is', () => {
    expect(previewAgeText(now - 8_000, now)).toBe('8 s ago');
    expect(previewAgeText(now - 180_000, now)).toBe('3 min ago');
  });
});

describe('run result toast (#15)', () => {
  test('names what the run did', () => {
    expect(
      lifecycleRunResultText('expire', { status: 'succeeded', objects_deleted: 80, objects_processed: 80, errors: 0 }),
    ).toEqual({ type: 'success', text: 'Lifecycle rule "expire" finished: deleted 80.' });
    expect(
      lifecycleRunResultText('expire', { status: 'paused', objects_deleted: 4, objects_processed: 4, errors: 1 }),
    ).toEqual({ type: 'warning', text: 'Lifecycle rule "expire" paused: deleted 4, 1 failed.' });
    expect(
      lifecycleRunResultText('expire', { status: 'deferred', objects_deleted: 4, objects_processed: 4, errors: 0 }),
    ).toEqual({
      type: 'warning',
      text: 'Lifecycle rule "expire" deferred by a maintenance job: deleted 4. It goes on in about a minute.',
    });
    expect(
      lifecycleRunResultText('expire', { status: 'succeeded', objects_deleted: 0, objects_processed: 0, errors: 0 }).text,
    ).toBe('Lifecycle rule "expire" finished: nothing to do.');
  });
});

describe('glob errors (#18)', () => {
  test('flags unclosed classes and alternations', () => {
    expect(globError('releases/**')).toBeNull();
    expect(globError('*.{zip,tar}')).toBeNull();
    expect(globError('[abc]*')).toBeNull();
    expect(globError('[abc')).toBe('"[abc" has a "[" without a "]"');
    expect(globError('*.{zip')).toBe('"*.{zip" has a "{" without a "}"');
    expect(globError('a}')).toBe('"a}" has a "}" without a "{"');
  });
  test('the field error names the rule', () => {
    expect(ruleGlobError('expire', ['ok/**', '[x'])).toBe(
      'Rule "expire": "[x" has a "[" without a "]". The proxy refuses this rule until you fix the glob.',
    );
    expect(ruleGlobError('expire', ['ok/**'])).toBeNull();
  });
});

describe('first run of a new rule (#6)', () => {
  test('parses the humantime tick', () => {
    expect(parseTickMs('1h')).toBe(3_600_000);
    expect(parseTickMs('2h30m')).toBe(9_000_000);
    expect(parseTickMs('10s')).toBe(60_000);
    expect(parseTickMs('wat')).toBeNull();
  });
  test('says when', () => {
    expect(firstRunText('1h', Date.parse('2026-10-13T10:00:00Z'))).toMatch(/^first scheduled run about 1h after you apply/);
    expect(firstRunText('nope', 0)).toBeNull();
  });
});

describe('newestSettledRun', () => {
  test('the newest settled run, by start time then id; a running one is skipped', () => {
    const runs = [
      { id: 1, started_at: 100, status: 'succeeded' },
      { id: 3, started_at: 200, status: 'running' },
      { id: 2, started_at: 150, status: 'failed' },
      { id: 4, started_at: 150, status: 'succeeded' },
    ];
    expect(newestSettledRun(runs)?.id).toBe(4);
    expect(newestSettledRun([{ id: 1, started_at: 1, status: 'queued' }])).toBeNull();
    expect(newestSettledRun([])).toBeNull();
  });
});
