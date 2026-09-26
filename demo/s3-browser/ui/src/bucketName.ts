/**
 * Why `name` is not a valid bucket name, or null. The same rules as the
 * server's `validate_bucket_name` (src/security.rs); the server stays the
 * authority. Empty is null: the form disables its button instead.
 */
export function bucketNameError(name: string): string | null {
  if (name === '') return null;
  if (!/^[a-z0-9.-]*$/.test(name)) return 'Use only lowercase letters, digits, dots and hyphens.';
  if (name.length < 3 || name.length > 63) return `A bucket name has 3 to 63 characters (this one has ${name.length}).`;
  if (name.includes('..')) return 'A bucket name cannot have two dots in a row.';
  if (!/^[a-z0-9]/.test(name) || !/[a-z0-9]$/.test(name)) return 'A bucket name must start and end with a letter or digit.';
  if (/^\d+(\.\d+){3}$/.test(name)) return 'A bucket name cannot look like an IP address.';
  return null;
}
