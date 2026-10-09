import { describe, expect, it } from 'vitest';
import { signInChoices } from '../signInChoices';

describe('signInChoices', () => {
  it('offers the admin password when no credential is left', () => {
    expect(signInChoices('deny_all')).toEqual({ primary: 'password', noCredentialNotice: true });
  });
  it('keeps the other modes as they were', () => {
    expect(signInChoices('bootstrap')).toEqual({ primary: 'password', noCredentialNotice: false });
    expect(signInChoices('iam')).toEqual({ primary: 'keys', noCredentialNotice: false });
    expect(signInChoices('open')).toEqual({ primary: 'auto', noCredentialNotice: false });
    expect(signInChoices(null)).toEqual({ primary: 'keys', noCredentialNotice: false });
  });
});
