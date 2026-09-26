/**
 * Browser review #11: a files-only session (access-key sign-in) may use the
 * bulk actions; the server authorizes each key with the user's permissions.
 */
import { describe, expect, test } from 'vitest';
import { deriveSessionCapabilities } from '../sessionCapabilities';

describe('deriveSessionCapabilities', () => {
  test('bulk actions for admin and files-only sessions, not signed-out', () => {
    expect(deriveSessionCapabilities({ valid: true, admin_gui: true }).canUseBulkActions).toBe(true);
    const filesOnly = deriveSessionCapabilities({ valid: true, admin_gui: false });
    expect(filesOnly.canUseBulkActions).toBe(true);
    expect(filesOnly.canFetchFullAdminConfig).toBe(false);
    expect(deriveSessionCapabilities({ valid: false, admin_gui: false }).canUseBulkActions).toBe(false);
  });
});
