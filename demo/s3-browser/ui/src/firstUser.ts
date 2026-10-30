/**
 * The first IAM user switches the proxy from bootstrap mode to IAM mode
 * (rebuild_iam_index in src/api/admin/users.rs): S3 clients and the admin
 * GUI then sign in with IAM user keys, and the running bootstrap key is kept
 * as the user `legacy-admin`. The create confirmation and the credentials
 * dialog both say so with this text.
 */
export function firstUserSwitchText(bootstrapAccessKey?: string): string {
  return bootstrapAccessKey
    ? `Sign-in changes to IAM users: the sign-in page asks for an access key and secret instead of the admin password. Your bootstrap key ${bootstrapAccessKey} becomes the user legacy-admin, with full access, so S3 clients that use it keep working.`
    : 'Sign-in changes to IAM users: the sign-in page asks for an access key and secret instead of the admin password. To open these settings later, give this user admin access.';
}

