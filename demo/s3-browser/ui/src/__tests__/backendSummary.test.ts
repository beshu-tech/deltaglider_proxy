/** src/backendSummary.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { backendSummary } from '../backendSummary';

test('an S3 backend names its access key id', () => {
  assert.equal(
    backendSummary({ backend_type: 's3', path: null, endpoint: 'https://fsn1.example', region: 'eu-central-1', access_key_id: 'AKHETZNER' }),
    's3: https://fsn1.example (eu-central-1) · key AKHETZNER',
  );
});

test('an S3 backend without a key id and a filesystem backend', () => {
  assert.equal(
    backendSummary({ backend_type: 's3', path: null, endpoint: null, region: 'us-east-1' }),
    's3: AWS (us-east-1)',
  );
  assert.equal(
    backendSummary({ backend_type: 'filesystem', path: '/srv/dg', endpoint: null, region: null }),
    'filesystem: /srv/dg',
  );
});
