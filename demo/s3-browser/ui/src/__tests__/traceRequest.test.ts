import assert from 'node:assert/strict';
import { test } from 'vitest';
import { buildTraceBody } from '../traceRequest';

// WIRE CONTRACT: this body is POSTed verbatim to /_/api/admin/config/trace.
// It must be byte-identical to the prior inline builder in TracePanel.run().

test('buildTraceBody: base shape, omits empty query/source_ip', () => {
  assert.deepEqual(
    buildTraceBody({ method: 'GET', path: '/', query: '', sourceIp: '', authenticated: false }),
    { method: 'GET', path: '/', authenticated: false },
  );
});

test('buildTraceBody: authenticated round-trips both booleans', () => {
  assert.deepEqual(
    buildTraceBody({ method: 'PUT', path: '/b/k', query: '', sourceIp: '', authenticated: true }),
    { method: 'PUT', path: '/b/k', authenticated: true },
  );
});

test('buildTraceBody: non-empty query is included, trimmed', () => {
  assert.deepEqual(
    buildTraceBody({ method: 'GET', path: '/b', query: '  prefix=x/ ', sourceIp: '', authenticated: false }),
    { method: 'GET', path: '/b', authenticated: false, query: 'prefix=x/' },
  );
});

test('buildTraceBody: non-empty source IP is included, trimmed', () => {
  assert.deepEqual(
    buildTraceBody({ method: 'GET', path: '/b', query: '', sourceIp: '  203.0.113.5 ', authenticated: false }),
    { method: 'GET', path: '/b', authenticated: false, source_ip: '203.0.113.5' },
  );
});

test('buildTraceBody: whitespace-only query / source_ip omit the key entirely', () => {
  const onlySpaces = buildTraceBody({
    method: 'GET',
    path: '/',
    query: '   ',
    sourceIp: '\t\n',
    authenticated: false,
  });
  assert.deepEqual(onlySpaces, { method: 'GET', path: '/', authenticated: false });
  assert.ok(!('query' in onlySpaces), 'whitespace-only query must not add the key');
  assert.ok(!('source_ip' in onlySpaces), 'whitespace-only source_ip must not add the key');
});

test('buildTraceBody: both present together', () => {
  assert.deepEqual(
    buildTraceBody({ method: 'DELETE', path: '/b/k', query: 'list-type=2', sourceIp: '2001:db8::1', authenticated: true }),
    { method: 'DELETE', path: '/b/k', authenticated: true, query: 'list-type=2', source_ip: '2001:db8::1' },
  );
});
