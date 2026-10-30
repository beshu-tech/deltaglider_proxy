/**
 * The rule editors check durations the way the server parses them
 * (humantime), so a typo shows under the field (UX harvest C3).
 */
import { describe, expect, test } from 'vitest';
import {
  durationError,
  durationSeconds,
  firstRuleFieldError,
  lifecycleRuleFieldErrors,
  replicationRuleFieldErrors,
} from '../components/jobs/humanDuration';
import type { LifecycleRuleConfig, ReplicationRuleConfig } from '../adminApi';

describe('durationSeconds', () => {
  test.each([
    ['30d', 30 * 86400],
    ['12h', 12 * 3600],
    ['90m', 90 * 60],
    ['1h 30m', 5400],
    ['1h30m', 5400],
    ['30 d', 30 * 86400],
    ['2months', 2 * 2_630_016],
    ['1M', 2_630_016],
    ['1.5h', 5400],
    ['0', 0],
    ['500ms', 0],
  ])('%s is %d seconds', (text, secs) => {
    expect(durationSeconds(text)).toBe(secs);
  });

  test.each(['notaduration', 'banana', '30', 'd30', '30x', '1H', '-5m', '1.h', '', ' '])('%j is refused', (text) => {
    expect(durationSeconds(text)).toBeNull();
  });
});

describe('durationError', () => {
  test('empty: an error unless optional', () => {
    expect(durationError('')).toBe('Enter a duration. Use a duration such as 30d, 12h or 90m.');
    expect(durationError('  ', { optional: true })).toBeNull();
  });
  test('a typo names the value', () => {
    expect(durationError('notaduration')).toBe('"notaduration" is not a duration. Use a duration such as 30d, 12h or 90m.');
  });
  test('below the minimum', () => {
    expect(durationError('10s', { min: { seconds: 30, text: '30s' } })).toBe('Enter 30s or more.');
    expect(durationError('30s', { min: { seconds: 30, text: '30s' } })).toBeNull();
  });
});

const lc = (over: Partial<LifecycleRuleConfig>): LifecycleRuleConfig =>
  ({ name: 'expire-old', enabled: true, bucket: 'releases', prefix: '', action: 'delete', expire_after: '30d', ...over }) as LifecycleRuleConfig;

describe('rule field errors', () => {
  test('lifecycle: expire_after must be a duration above 0', () => {
    expect(lifecycleRuleFieldErrors(lc({}))).toEqual({});
    expect(lifecycleRuleFieldErrors(lc({ expire_after: 'notaduration' })).expire_after).toMatch(/is not a duration/);
    expect(lifecycleRuleFieldErrors(lc({ expire_after: '0' })).expire_after).toBe('Enter 1s or more.');
  });
  test('lifecycle retain-newest: the optional ages, not expire_after', () => {
    const rule = lc({ expire_after: 'junk', action: { type: 'retain-newest', count: 2, qualify: { min_age: '1x' }, protect_younger_than: '7d' } });
    expect(lifecycleRuleFieldErrors(rule)).toEqual({ min_age: '"1x" is not a duration. Use a duration such as 30d, 12h or 90m.' });
  });
  test('replication: interval is optional, minimum 30s', () => {
    const r = (interval: string) => ({ name: 'nightly', interval }) as ReplicationRuleConfig;
    expect(replicationRuleFieldErrors(r('24h'))).toEqual({});
    expect(replicationRuleFieldErrors(r(''))).toEqual({});
    expect(replicationRuleFieldErrors(r('banana')).interval).toMatch(/"banana" is not a duration/);
    expect(replicationRuleFieldErrors(r('10s')).interval).toBe('Enter 30s or more.');
  });
  test('the first error names the rule and the field', () => {
    expect(firstRuleFieldError([lc({}), lc({ name: 'qa-lc-rule', expire_after: 'x' })], lifecycleRuleFieldErrors)).toBe(
      'Rule qa-lc-rule, Expire after: "x" is not a duration. Use a duration such as 30d, 12h or 90m.',
    );
    expect(firstRuleFieldError([lc({})], lifecycleRuleFieldErrors)).toBeNull();
  });
});
