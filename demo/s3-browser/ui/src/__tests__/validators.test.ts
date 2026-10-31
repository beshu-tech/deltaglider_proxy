/** The field validators (UX harvest D9, D10, D46, D59): one per field kind. */
import { describe, expect, test } from 'vitest';
import { parseByteSize, v } from '../validation';

const ok = (validator: (t: string) => string | null, ...texts: string[]) =>
  texts.forEach((t) => expect(validator(t), t).toBeNull());
const bad = (validator: (t: string) => string | null, ...texts: string[]) =>
  texts.forEach((t) => expect(validator(t), t).toEqual(expect.any(String)));

describe('integer never rounds or clamps', () => {
  const cache = v.integer({ min: 16, max: 65536 });
  test('good', () => ok(cache, '16', '1024', ' 65536 '));
  test('bad', () => bad(cache, '-1', '15', '65537', 'abc', '1e3', '12.5', ''));
  test('the sentence names the range', () => {
    expect(cache('-1')).toBe('Enter a whole number between 16 and 65,536.');
  });
  test('optional: empty is the default', () => expect(v.optional(cache)('')).toBeNull());
});

test('hostPort', () => {
  const hp = v.hostPort();
  ok(hp, '0.0.0.0:9000', '127.0.0.1:1', '[::]:9000', '[2001:db8::1]:443', 'proxy.internal:9000', 'localhost:80');
  bad(hp, 'not an address', '0.0.0.0', ':9000', '0.0.0.0:0', '0.0.0.0:70000', '999.1.1.1:80', '::1:9000', 'a b:80');
});

test('ip and ipOrCidr', () => {
  ok(v.ip(), '203.0.113.7', '2001:db8::1');
  bad(v.ip(), '999.1.1.1', '10.0.0.0/8', 'x');
  ok(v.ipOrCidr(), '10.0.0.0/8', '2001:db8::/32', '1.2.3.4');
  bad(v.ipOrCidr(), '999.1.1.1', '10.0.0.0/33');
});

test('url', () => {
  ok(v.url(), 'https://accounts.google.com', 'http://keycloak:8080/realms/x');
  bad(v.url(), 'not a url', 'ftp://x', 'accounts.google.com');
  bad(v.url({ httpsOnly: true }), 'http://x');
});

test('names', () => {
  ok(v.ruleName(), 'qa-lc-rule', 'a.b_c-1');
  bad(v.ruleName(), 'qa bad name!', '', 'x'.repeat(65));
  expect(v.ruleName()('qa bad name!')).toMatch(/^Remove the spaces/);
  ok(v.iamName(), 'ci-uploader', 'dana@example.com');
  bad(v.iamName(), 'qa group with spaces!', '');
  ok(v.bucketName(), 'releases');
  bad(v.bucketName(), 'Releases', '', 'ab');
  ok(v.admissionBlockName(), 'deny-bots:v1');
  bad(v.admissionBlockName(), 'public-prefix:x', 'a b');
});

test('duration and required', () => {
  ok(v.duration(), '30d', '12h 30m');
  bad(v.duration(), 'notaduration', '');
  expect(v.required('a bucket')('  ')).toBe('Enter a bucket.');
  expect(v.all(v.required('a bucket'), v.bucketName())('')).toBe('Enter a bucket.');
});

test('size and parseByteSize', () => {
  expect(parseByteSize('10 GiB')).toBe(10 * 2 ** 30);
  expect(parseByteSize('512MB')).toBe(512e6);
  expect(parseByteSize('1e3')).toBeNull();
  ok(v.size(), '1 KiB', '100 GiB', '4096');
  bad(v.size(), '0', 'abc', '-1 GiB', '1e3');
  expect(v.size({ maxBytes: 2 ** 50, maxText: '1 PiB' })('2 PiB')).toBe('Enter at most 1 PiB.');
});
