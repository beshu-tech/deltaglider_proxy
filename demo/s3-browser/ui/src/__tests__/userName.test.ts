// The GUI user-name rule matches the server's (user_name_error in
// src/api/admin/users.rs, which has the same truth table).
import { expect, test } from 'vitest';
import { userNameError } from '../userName';

test.each([
  'ci-uploader',
  'backup-bot',
  'dana',
  'dana@example.com',
  'a+b=c,d.e_f-g',
  'X',
  'a'.repeat(64),
])('%s is valid', (name) => {
  expect(userNameError(name)).toBeNull();
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
  expect(userNameError(name)).toMatch(why);
});
