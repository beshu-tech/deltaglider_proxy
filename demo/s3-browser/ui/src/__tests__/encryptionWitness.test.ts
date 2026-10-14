/**
 * Issue #116 P3: the rules of the encryption witness view. A mismatch maps
 * to the publish subject that the server accepts (a bucket name,
 * `route:default`, `backend:<name>`), a plaintext form needs its downgrade
 * confirmation, and a bucket card finds the mismatch that refuses its
 * writes as the gate does (own entry first, else the backend's `*`).
 */
import { expect, test } from 'vitest';
import {
  bucketMismatch,
  describeIntent,
  publishPlan,
  publishSubject,
  storageLabel,
  type WitnessConfig,
  type WitnessIntent,
  type WitnessMismatch,
} from '../encryptionWitness';
import type { BackendInfo } from '../adminApi';

const backend = (name: string): BackendInfo => ({
  name, backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null, has_credentials: true,
  encryption: { mode: 'none', has_key: false, shim_active: false },
});

const CFG: WitnessConfig = {
  backends: [backend('hetzner-fsn1'), backend('local-disk'), backend('aws-dr')],
  default_backend: 'hetzner-fsn1',
  bucket_policies: {
    releases: { backend: 'hetzner-fsn1' },
    'db-archive': { backend: 'local-disk', alias: 'archive-2026' },
    downloads: { public: true },
  },
};

const AES: WitnessIntent = { mode: 'aes256-gcm-proxy', key_id: 'k1', kcv: '0011223344556677' };
const PLAIN: WitnessIntent = { mode: 'none' };

const m = (backendName: string, bucket: string, local = AES, published = PLAIN): WitnessMismatch => ({
  backend: backendName, bucket, local, published, set_by: 'pod-a',
});

test('a placed bucket publishes by its name, through its real name (alias)', () => {
  expect(publishSubject(m('hetzner-fsn1', 'releases'), CFG)).toBe('releases');
  expect(publishSubject(m('local-disk', 'archive-2026'), CFG)).toBe('db-archive');
});

test('the default backend\'s other buckets publish as route:default, another backend\'s as backend:<name>', () => {
  expect(publishSubject(m('hetzner-fsn1', '*'), CFG)).toBe('route:default');
  expect(publishSubject(m('aws-dr', '*'), CFG)).toBe('backend:aws-dr');
});

test('a bucket that the config does not place publishes through its backend', () => {
  expect(publishSubject(m('hetzner-fsn1', 'downloads'), CFG)).toBe('backend:hetzner-fsn1');
});

test('with no named backends a bucket is on the backend "default" by its own name', () => {
  const single: WitnessConfig = { backends: [], default_backend: null, bucket_policies: {} };
  expect(publishSubject(m('default', 'releases'), single)).toBe('releases');
  expect(publishSubject(m('default', '*'), single)).toBe('route:default');
});

test('a plaintext form is a downgrade to confirm; one subject is sent once', () => {
  const plan = publishPlan(
    [m('hetzner-fsn1', 'releases', PLAIN, AES), m('aws-dr', '*'), m('aws-dr', 'x', PLAIN, AES), m('aws-dr', 'y')],
    CFG,
  );
  expect(plan.storages).toEqual(['releases', 'backend:aws-dr']);
  expect(plan.downgrades).toEqual(['releases', 'backend:aws-dr']);
  expect(publishPlan([m('hetzner-fsn1', 'releases')], CFG).downgrades).toEqual([]);
});

test('a bucket card finds its own mismatch, else its backend\'s "*" when the config does not place it', () => {
  const all = [m('hetzner-fsn1', '*'), m('local-disk', 'archive-2026')];
  expect(bucketMismatch(all, CFG, 'db-archive', 'local-disk')).toBe(all[1]);
  // releases is placed: judged on its own entry, which agrees.
  expect(bucketMismatch(all, CFG, 'releases', 'hetzner-fsn1')).toBeNull();
  // downloads has a policy without a route: it follows its backend's "*".
  expect(bucketMismatch(all, CFG, 'downloads', 'hetzner-fsn1')).toBe(all[0]);
  expect(bucketMismatch(all, CFG, 'other', 'hetzner-fsn1')).toBe(all[0]);
  expect(bucketMismatch(all, CFG, 'other', 'aws-dr')).toBeNull();
  expect(bucketMismatch([], CFG, 'other', 'hetzner-fsn1')).toBeNull();
});

test('the view names an intent and the "*" storage in words', () => {
  expect(describeIntent(PLAIN)).toBe('none (plaintext)');
  expect(describeIntent(AES)).toBe('aes256-gcm-proxy · key k1 (check 0011223344556677)');
  expect(describeIntent({ mode: 'aes256-gcm-proxy' })).toBe('aes256-gcm-proxy · no usable key');
  expect(describeIntent({ mode: 'sse-kms', key_id: 'arn:aws:kms:k' })).toBe('sse-kms · key arn:aws:kms:k');
  expect(storageLabel(m('aws-dr', '*'))).toBe('All buckets without their own setting');
  expect(storageLabel(m('aws-dr', 'releases'))).toBe('releases');
});
