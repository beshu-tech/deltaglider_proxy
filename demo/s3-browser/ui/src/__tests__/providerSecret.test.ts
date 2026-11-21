import { expect, test } from 'vitest';
import { savingClearsTheSecret, withNetworkPolicy } from '../providerSecret';

const saved = { issuer_url: 'https://idp.acme.example', extra_config: { hd: 'acme.example' } };
const same = { issuerUrl: 'https://idp.acme.example', allowLocal: false, caCertPath: '' };

test('an edit that moves no destination keeps the secret', () => {
  expect(savingClearsTheSecret(saved, same, '')).toBe(false);
  expect(savingClearsTheSecret({ issuer_url: 'https://a', extra_config: null }, { ...same, issuerUrl: 'https://a' }, '')).toBe(false);
});

test('a moved issuer with a blank secret clears it', () => {
  expect(savingClearsTheSecret(saved, { ...same, issuerUrl: 'https://other.example' }, '')).toBe(true);
});

test('a blank issuer is sent as "keep", so it clears nothing', () => {
  expect(savingClearsTheSecret(saved, { ...same, issuerUrl: '' }, '')).toBe(false);
});

test('the issuer is compared as typed: the server does not trim', () => {
  expect(savingClearsTheSecret(saved, { ...same, issuerUrl: ' https://idp.acme.example' }, '')).toBe(true);
  expect(savingClearsTheSecret(saved, { ...same, issuerUrl: ' ' }, '')).toBe(true);
});

test('a typed secret is never cleared', () => {
  expect(savingClearsTheSecret(saved, { ...same, issuerUrl: 'https://other.example' }, 'new')).toBe(false);
});

test('a changed network policy with a blank secret clears it', () => {
  expect(savingClearsTheSecret(saved, { ...same, allowLocal: true }, '')).toBe(true);
  expect(savingClearsTheSecret(saved, { ...same, caCertPath: '/etc/ca.pem' }, '')).toBe(true);
});

test('the network policy keeps the other extra_config keys', () => {
  expect(withNetworkPolicy({ hd: 'x', allow_local: true }, false, '')).toEqual({ hd: 'x' });
  expect(withNetworkPolicy(null, true, ' /ca.pem ')).toEqual({ allow_local: true, ca_cert_path: '/ca.pem' });
});
