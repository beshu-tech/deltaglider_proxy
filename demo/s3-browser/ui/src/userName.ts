/**
 * THE user-name rule, the same as the server's (`user_name_error` in
 * src/api/admin/users.rs): 1 to 64 characters, each a letter, a digit or one
 * of `+ = , . @ _ -` (the AWS IAM user-name set), and not `.` or `..`.
 * The server refuses a name outside it with 400; the form checks it first so
 * the error shows on the field.
 */
const USER_NAME_MAX_LEN = 64;

const USER_NAME_CHARS = /^[A-Za-z0-9+=,.@_-]*$/;

export const USER_NAME_HELP = 'Letters, digits and + = , . @ _ - only. No spaces. Up to 64 characters.';

/** The sentence that says why `name` is not a valid user name, or null. */
export function userNameError(name: string): string | null {
  if (name.trim() === '') return 'Enter a name.';
  if (name.startsWith('$')) return 'A name cannot start with $.';
  if ([...name].length > USER_NAME_MAX_LEN) return `Use ${USER_NAME_MAX_LEN} characters or fewer.`;
  if (!USER_NAME_CHARS.test(name)) {
    return /\s/.test(name)
      ? 'Remove the spaces. Use letters, digits and + = , . @ _ - only.'
      : 'Use letters, digits and + = , . @ _ - only.';
  }
  if (name === '.' || name === '..') return 'A name cannot be . or ..';
  return null;
}
