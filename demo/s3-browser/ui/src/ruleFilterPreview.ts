/**
 * Pure words and shapes of the rule filter preview (RuleFilterFields).
 *
 * React-free so the node unit project tests it. The preview itself comes
 * from the server (`previewFilters`), which runs the rules' own matcher.
 */
import type { FilterPreview, FilterPreviewGlob, FilterVerdict } from './adminApi/jobs';
import { noun } from './utils';

const count = (n: number) => n.toLocaleString('en-US');

/** `bucket/prefix`, for the sentences. */
export function scopeLabel(bucket: string, prefix: string): string {
  return `${bucket}/${prefix}`;
}

/**
 * One sentence on what the filters do to the sample. `verb` is what an
 * included object does: "replicate", "are candidates".
 */
export function filterSummary(p: FilterPreview, scope: string, verb: string): string {
  if (p.sampled === 0) return `No objects under ${scope} yet. The filters apply to objects as they arrive.`;
  const of = p.truncated
    ? `Of the first ${count(p.sampled)} objects under ${scope}`
    : `Of the ${count(p.sampled)} ${noun(p.sampled, 'object')} under ${scope}`;
  const parts = [`${count(p.included)} ${verb}`];
  if (p.excluded > 0) parts.push(`${count(p.excluded)} ${p.excluded === 1 ? 'is' : 'are'} excluded`);
  if (p.not_included > 0) parts.push(`${count(p.not_included)} ${p.not_included === 1 ? 'matches' : 'match'} no include glob`);
  if (p.internal > 0) parts.push(`${count(p.internal)} ${p.internal === 1 ? 'is' : 'are'} proxy-internal`);
  const list = parts.length > 1 ? `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}` : parts[0];
  return `${of}, ${list}.`;
}

/** The server's report on one glob of one list, if it has one. */
export function globReport(p: FilterPreview | undefined, list: 'include' | 'exclude', glob: string): FilterPreviewGlob | undefined {
  return p?.globs.find((g) => g.list === list && g.glob === glob);
}

/** The words for one key's verdict. */
export function verdictText(verdict: FilterVerdict, glob: string | undefined, verb: string): string {
  switch (verdict) {
    case 'included':
      return glob ? `${verbSingular(verb)}: matches ${glob}` : verbSingular(verb);
    case 'excluded':
      return `Excluded by ${glob}`;
    case 'not_included':
      return 'Matches no include glob';
    case 'internal':
      return 'Proxy-internal: never acted on';
  }
}

/** "replicate" → "Replicates"; "are candidates" → "Is a candidate". */
function verbSingular(verb: string): string {
  if (verb === 'replicate') return 'Replicates';
  if (verb === 'are candidates') return 'Is a candidate';
  return verb;
}

/** The key split at the prefix: the prefix shows dimmed. */
export function splitAtPrefix(key: string, prefix: string): [string, string] {
  return prefix && key.startsWith(prefix) ? [prefix, key.slice(prefix.length)] : ['', key];
}

/**
 * A rule's prefix as the proxy reads it (`normalize_prefix`): no empty
 * segments, a trailing `/`. The preview samples this folder.
 */
export function ruleFolderPrefix(prefix: string): string {
  const parts = prefix.split('/').filter(Boolean);
  return parts.length ? `${parts.join('/')}/` : '';
}
