import assert from 'node:assert/strict';
import { test } from 'vitest';
import { undefinedToNullSubset } from '../components/advancedPayload';

test('a cleared scalar becomes explicit null; a set scalar is untouched', () => {
  const result = undefinedToNullSubset(
    { cache_size_mb: undefined, metadata_cache_mb: 50 },
    ['cache_size_mb', 'metadata_cache_mb'],
  );
  assert.deepEqual(result, { cache_size_mb: null, metadata_cache_mb: 50 });
});

test('THE core bug: JSON round-trip must PRESERVE the null (not drop the key)', () => {
  const result = undefinedToNullSubset(
    { cache_size_mb: undefined, metadata_cache_mb: 50 },
    ['cache_size_mb', 'metadata_cache_mb'],
  );
  const roundTripped = JSON.parse(JSON.stringify(result)) as Record<string, unknown>;
  assert.ok('cache_size_mb' in roundTripped, 'cleared key must survive JSON.stringify');
  assert.equal(roundTripped.cache_size_mb, null);
  assert.equal(roundTripped.metadata_cache_mb, 50);
});

test('listen_addr clear over the listener subset → both null; stringify keeps it', () => {
  const result = undefinedToNullSubset(
    { listen_addr: undefined, tls: undefined },
    ['listen_addr', 'tls'],
  );
  assert.deepEqual(result, { listen_addr: null, tls: null });
  const json = JSON.stringify(result);
  assert.ok(json.includes('"listen_addr":null'), 'listen_addr null must survive stringify');
});

test('a populated tls object passes through unchanged (no recursion, not nulled)', () => {
  const tls = { enabled: true, cert_path: '/x' };
  const result = undefinedToNullSubset(
    { listen_addr: undefined, tls },
    ['listen_addr', 'tls'],
  );
  assert.equal(result.tls, tls, 'populated tls must pass through by reference');
  assert.equal(result.listen_addr, null);
});

test('a set scalar passes through unchanged', () => {
  const result = undefinedToNullSubset(
    { config_sync_bucket: 'mybucket' },
    ['config_sync_bucket'],
  );
  assert.deepEqual(result, { config_sync_bucket: 'mybucket' });
});
