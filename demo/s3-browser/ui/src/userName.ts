/**
 * THE IAM name rule for user AND group names, the same as the server's
 * (`iam_name_error` in src/api/admin/users.rs): 1 to 64 characters, each a
 * letter, a digit or one of `+ = , . @ _ -` (the AWS IAM user-name set), and
 * not `.` or `..`. The server refuses a name outside it with 400; the forms
 * check it first so the error shows on the field.
 */
const IAM_NAME_MAX_LEN = 64;

const IAM_NAME_CHARS = /^[A-Za-z0-9+=,.@_-]*$/;

export const IAM_NAME_HELP = 'Letters, digits and + = , . @ _ - only. No spaces. Up to 64 characters.';

/** The sentence that says why `name` is not a valid user or group name, or null. */
export function iamNameError(name: string): string | null {
  if (name.trim() === '') return 'Enter a name.';
  if (name.startsWith('$')) return 'A name cannot start with $.';
  if ([...name].length > IAM_NAME_MAX_LEN) return `Use ${IAM_NAME_MAX_LEN} characters or fewer.`;
  if (!IAM_NAME_CHARS.test(name)) {
    return /\s/.test(name)
      ? 'Remove the spaces. Use letters, digits and + = , . @ _ - only.'
      : 'Use letters, digits and + = , . @ _ - only.';
  }
  if (name === '.' || name === '..') return 'A name cannot be . or ..';
  return null;
}

/**
 * The server's 400 name refusal ("user name …" / "group name …") as a field
 * sentence, or null when `detail` is another error.
 */
export function serverNameErrorText(kind: 'user' | 'group', detail: string): string | null {
  if (!detail.toLowerCase().startsWith(`${kind} name `)) return null;
  return `${detail.charAt(0).toUpperCase()}${detail.slice(1)}.`;
}
