/**
 * The server keeps a provider's client secret only while the provider's
 * destination stays the same: a changed issuer URL or network policy
 * (`extra_config`) with no new secret in the same request clears it. These
 * helpers say, before the save, when that will happen.
 */

type ExtraConfig = Record<string, unknown> | null | undefined;

/** The network policy a form sets, merged over the saved `extra_config`. */
export function withNetworkPolicy(
  base: ExtraConfig,
  allowLocal: boolean,
  caCertPath: string,
): Record<string, unknown> {
  const next: Record<string, unknown> = { ...(base ?? {}) };
  delete next.allow_local;
  delete next.ca_cert_path;
  if (allowLocal) next.allow_local = true;
  if (caCertPath.trim()) next.ca_cert_path = caCertPath.trim();
  return next;
}

/** JSON with sorted keys: the same object always gives the same text. */
function stable(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(stable).join(',')}]`;
  if (value && typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) => a.localeCompare(b));
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${stable(v)}`).join(',')}}`;
  }
  return JSON.stringify(value) ?? 'null';
}

/**
 * True when saving this form moves the destination of a saved provider and
 * the secret field is blank: the server then clears the stored secret.
 */
export function savingClearsTheSecret(
  saved: { issuer_url?: string | null; extra_config?: ExtraConfig },
  form: { issuerUrl: string; allowLocal: boolean; caCertPath: string },
  typedSecret: string,
): boolean {
  if (typedSecret !== '') return false;
  // The form sends a blank issuer as `undefined` (keep); the server compares the text as sent.
  const issuerMoved = form.issuerUrl !== '' && form.issuerUrl !== (saved.issuer_url ?? '');
  const policyMoved =
    stable(withNetworkPolicy(saved.extra_config, form.allowLocal, form.caCertPath)) !==
    stable(saved.extra_config ?? {});
  return issuerMoved || policyMoved;
}
