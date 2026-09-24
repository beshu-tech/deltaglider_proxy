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

/** Entries separated by new lines, commas or spaces (a pasted list). */
export function parseSourceIpLines(text: string): string[] {
  return text.split(/[\s,]+/).filter((l) => l.length > 0);
}

const V4_OCTET = '(25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)';
const V4 = new RegExp(`^${V4_OCTET}(\\.${V4_OCTET}){3}$`);

function isIpv4(s: string): boolean {
  return V4.test(s);
}

function isIpv6(s: string): boolean {
  if (!s.includes(':') || !/^[0-9a-fA-F:.]+$/.test(s)) return false;
  // The URL parser implements the full IPv6 grammar (::, embedded IPv4).
  try {
    new URL(`http://[${s}]/`);
    return true;
  } catch {
    return false;
  }
}

/** Why one entry is not an IP address or a network (CIDR), or null. */
function sourceIpEntryError(entry: string): string | null {
  if (entry.length > 64) return 'is longer than 64 characters';
  const [addr, prefix, extra] = entry.split('/');
  const v4 = isIpv4(addr);
  if (extra !== undefined || (!v4 && !isIpv6(addr))) {
    return 'is not an IP address or a network such as 10.0.0.0/8';
  }
  if (prefix !== undefined) {
    const max = v4 ? 32 : 128;
    // No leading zeros: the server rejects /008.
    if (!/^(0|[1-9]\d{0,2})$/.test(prefix) || Number(prefix) > max) {
      return `has a network size that is not between /0 and /${max}`;
    }
  }
  return null;
}

/** Most entries the server accepts in `match.source_ip_list`. */
const MAX_SOURCE_IPS = 4096;

/**
 * The first problem in the field's text, with its line number, or null.
 * The textarea shows it; the rule cannot be saved while it is set.
 */
export function sourceIpProblem(text: string): string | null {
  const lines = text.split('\n');
  let count = 0;
  for (let i = 0; i < lines.length; i++) {
    for (const entry of parseSourceIpLines(lines[i])) {
      count++;
      const why = sourceIpEntryError(entry);
      if (why) return `Line ${i + 1}: "${entry.length > 40 ? `${entry.slice(0, 40)}…` : entry}" ${why}.`;
    }
  }
  if (count > MAX_SOURCE_IPS) return `Enter at most ${MAX_SOURCE_IPS} source IPs (there are ${count}).`;
  return null;
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
