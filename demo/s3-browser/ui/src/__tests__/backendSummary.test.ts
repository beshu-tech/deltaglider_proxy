/** src/backendSummary.ts */
import { expect, test } from 'vitest';
import { backendFacts, backendKind } from '../backendSummary';

test('an S3 backend lists its endpoint, region and access key id', () => {
  const b = { backend_type: 's3', path: null, endpoint: 'https://fsn1.example', region: 'eu-central-1', access_key_id: 'AKHETZNER' } as const;
  expect(backendKind(b)).toBe('S3-compatible');
  expect(backendFacts(b)).toEqual([
    { label: 'Endpoint', value: 'https://fsn1.example', mono: true },
    { label: 'Region', value: 'eu-central-1', mono: true },
    { label: 'Access key', value: 'AKHETZNER', mono: true },
  ]);
});

test('AWS without an endpoint, and no key id: no empty rows', () => {
  const b = { backend_type: 's3', path: null, endpoint: null, region: 'us-east-1' } as const;
  expect(backendKind(b)).toBe('Amazon S3');
  expect(backendFacts(b)).toEqual([
    { label: 'Endpoint', value: 'AWS default', mono: false },
    { label: 'Region', value: 'us-east-1', mono: true },
  ]);
});

test('a filesystem backend shows its path', () => {
  const b = { backend_type: 'filesystem', path: '/srv/dg', endpoint: null, region: null } as const;
  expect(backendKind(b)).toBe('Local filesystem');
  expect(backendFacts(b)).toEqual([{ label: 'Path', value: '/srv/dg', mono: true }]);
});
