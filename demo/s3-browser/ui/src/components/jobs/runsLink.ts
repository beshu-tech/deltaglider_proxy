import { buildViewUrl } from '../../urlState';

/**
 * The Jobs page (every run), filtered. `rule` is a rule's job id
 * (`replication:<name>`), `bucket` a bucket name, `job` the run or one-off
 * whose drawer opens.
 */
export function runsUrl(filter: { rule?: string; bucket?: string; job?: string } = {}): string {
  const query: Record<string, string> = {};
  for (const [k, v] of Object.entries(filter)) if (v) query[k] = v;
  return buildViewUrl('admin', 'jobs', Object.keys(query).length > 0 ? query : undefined);
}
