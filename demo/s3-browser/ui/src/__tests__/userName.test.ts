// The GUI user and group name rule matches the server's (iam_name_error in
// src/api/admin/users.rs, which has the same truth table).
import { expect, test } from 'vitest';
import { iamNameError } from '../userName';

test.each([
  'ci-uploader',
  'backup-bot',
  'dana',
  'dana@example.com',
  'a+b=c,d.e_f-g',
  'X',
  'a'.repeat(64),
])('%s is valid', (name) => {
  expect(iamNameError(name)).toBeNull();
});

test.each([
  ['', /Enter a name/],
  ['   ', /Enter a name/],
  ['$x', /cannot start with \$/],
  ['bad name with spaces!!', /Remove the spaces/],
  ['slash/name', /letters, digits/],
  ['ünïcode', /letters, digits/],
  ['.', /cannot be \. or \.\./],
  ['..', /cannot be \. or \.\./],
  ['a'.repeat(65), /64 characters or fewer/],
])('%j is refused', (name, why) => {
  expect(iamNameError(name)).toMatch(why);
});
