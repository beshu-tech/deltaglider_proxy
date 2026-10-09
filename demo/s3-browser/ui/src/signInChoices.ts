/**
 * Which sign-in the connect page and the re-login prompt offer, decided
 * from the whoami `mode` alone. React-free so src/__tests__/signInChoices.test.ts
 * can check every mode.
 */
import type { WhoamiResponse } from './adminApi';

export interface SignInChoices {
  /** The form shown first: the admin password, S3 access keys, or none (open access connects by itself). */
  primary: 'password' | 'keys' | 'auto';
  /** No IAM user and no bootstrap pair exist: say why S3 refuses every request. */
  noCredentialNotice: boolean;
}

export function signInChoices(mode: WhoamiResponse['mode'] | null): SignInChoices {
  switch (mode) {
    case 'open':
      return { primary: 'auto', noCredentialNotice: false };
    case 'bootstrap':
      return { primary: 'password', noCredentialNotice: false };
    // No credential is left (review A14): only the admin password can sign
    // in, to create an IAM user.
    case 'deny_all':
      return { primary: 'password', noCredentialNotice: true };
    default:
      return { primary: 'keys', noCredentialNotice: false };
  }
}
