/**
 * One "Source IPs" text field for a request rule, instead of two mutually
 * exclusive inputs (`match.source_ip` for one address, `match.source_ip_list`
 * for many). The YAML keeps both keys; this maps between them. Pure,
 * unit-tested.
 */
export interface SourceIpMatch {
  source_ip?: string;
  source_ip_list?: string[];
}

/** One entry per line, blank lines and surrounding space dropped. */
export function parseSourceIpLines(text: string): string[] {
  return text
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.length > 0);
}

/** The text shown in the field for a rule's match. */
export function sourceIpText(match: SourceIpMatch): string {
  if (match.source_ip_list && match.source_ip_list.length > 0) return match.source_ip_list.join('\n');
  return match.source_ip ?? '';
}

/**
 * The match keys for the field's text. A single plain address becomes
 * `source_ip`; anything else (a network, or several entries) becomes
 * `source_ip_list`. When the entries equal the rule's original ones, the
 * original key is kept so an unrelated edit does not rewrite the YAML.
 */
export function sourceIpMatch(text: string, original?: SourceIpMatch): SourceIpMatch {
  const entries = parseSourceIpLines(text);
  if (entries.length === 0) return {};
  if (original) {
    const before = parseSourceIpLines(sourceIpText(original));
    const same = before.length === entries.length && before.every((e, i) => e === entries[i]);
    if (same) {
      return original.source_ip_list && original.source_ip_list.length > 0
        ? { source_ip_list: entries }
        : { source_ip: entries[0] };
    }
  }
  if (entries.length === 1 && !entries[0].includes('/')) return { source_ip: entries[0] };
  return { source_ip_list: entries };
}
