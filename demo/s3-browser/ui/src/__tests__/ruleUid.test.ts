/**
 * Rule uids (src/ruleUid.ts): the Jobs editor identifies a rule by a stable
 * client-side uid, never by its name. The uid must never reach the server.
 */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, test } from 'vitest';
import { carryRuleUids, ruleUid, withRuleUid } from '../ruleUid';
import { buildReplicationPayload, DEFAULT_REPLICATION, emptyRule as emptyReplicationRule, normalizeReplication } from '../components/replicationPayload';
import { buildLifecyclePayload, DEFAULT_LIFECYCLE, emptyRule as emptyLifecycleRule, normalizeLifecycle } from '../components/lifecyclePayload';

describe('ruleUid', () => {
  test('an edit (object spread) keeps the uid; JSON drops it', () => {
    const r = withRuleUid({ name: 'a' }, 'uid-marker');
    expect(ruleUid({ ...r, name: 'b' })).toBe('uid-marker');
    expect(JSON.stringify(r)).toBe('{"name":"a"}');
  });

  test('every new or normalized rule has its own uid', () => {
    const a = emptyReplicationRule([]);
    const b = emptyReplicationRule([a]);
    expect(ruleUid(a)).not.toBe(ruleUid(b));
    const n = normalizeLifecycle({ rules: [{ name: 'x', bucket: 'y' } as never, { name: 'z', bucket: 'y' } as never] });
    expect(ruleUid(n.rules[0])).not.toBe(ruleUid(n.rules[1]));
  });

  test('carryRuleUids: same name = same uid; a uid is given once; other rules get new ones', () => {
    const prev = [withRuleUid({ name: 'a' }, 'u-a'), withRuleUid({ name: 'b' }, 'u-b'), withRuleUid({ name: 'a' }, 'u-a2')];
    const next = carryRuleUids([{ name: 'b' }, { name: 'a' }, { name: 'c' }], prev);
    expect(next.map(ruleUid).slice(0, 2)).toEqual(['u-b', 'u-a']);
    expect(['u-a', 'u-b', 'u-a2']).not.toContain(ruleUid(next[2]));
  });

  test('the uid never reaches the PUT body', () => {
    const repl = { ...DEFAULT_REPLICATION, rules: [withRuleUid({ ...emptyReplicationRule([]), source: { bucket: 's', prefix: '' }, destination: { bucket: 'd', prefix: '' } }, 'uid-marker')] };
    const r = buildReplicationPayload(normalizeReplication(repl));
    expect(r.ok).toBe(true);
    expect(JSON.stringify(r.ok && r.body)).not.toContain('uid-marker');
    const lc = { ...DEFAULT_LIFECYCLE, rules: [withRuleUid({ ...emptyLifecycleRule([]), bucket: 'b' }, 'uid-marker')] };
    const l = buildLifecyclePayload(lc);
    expect(l.ok).toBe(true);
    expect(JSON.stringify(l.ok && l.body)).not.toContain('uid-marker');
  });
});

/**
 * Source guard: the Jobs editor finds an EDITOR rule by uid
 * (findEditorRule / ruleUid), never by name. A lookup in a saved snapshot
 * (`baseline`) by server name is fine: the server knows rules by name.
 */
test('the Jobs editor never looks up an editor rule by its name', () => {
  const dir = join(__dirname, '../components/jobs');
  const byName = /\.rules\.(find|findIndex|filter|some|map)\(\(\w+(, \w+)?\) => \w+\.name\s*[!=]==/;
  for (const f of ['JobsPanel.tsx', 'JobDrawer.tsx']) {
    const bad = readFileSync(join(dir, f), 'utf8')
      .split('\n')
      .filter((l) => byName.test(l) && !/baseline/.test(l));
    expect(bad, f).toEqual([]);
  }
});
