/**
 * THE field validators of the UI (UX harvest D9, D10, D46, D59).
 *
 * A validator is a pure `(text) => error sentence | null`. It reads the
 * TEXT the user typed, never a clamped or parsed copy, so "-1" is refused
 * with a sentence instead of becoming 16. Each one mirrors the server's own
 * rule (named in its doc); the server stays the authority.
 *
 *   import { v } from '../validation';
 *   v.integer({ min: 16, max: 65536 })('-1')  // 'Enter a whole number between 16 and 65,536.'
 *   v.optional(v.hostPort())('')               // null: empty means "the default"
 *   v.all(v.required('a rule name'), v.ruleName())
 *
 * Wire a validator to a field with `useFieldValidation` (./useFieldValidation).
 */
import { iamNameError } from '../userName';
import { bucketNameError } from '../bucketName';
import { durationError } from '../components/jobs/humanDuration';
import { isIpv4, isIpv6, sourceIpEntryError } from '../sourceIpField';

export type Validator = (text: string) => string | null;

const fmt = (n: number) => n.toLocaleString('en-US');

/** The first error of several validators, in order. */
export function all(...validators: Validator[]): Validator {
  return (text) => {
    for (const check of validators) {
      const error = check(text);
      if (error) return error;
    }
    return null;
  };
}

/** Empty text is valid (the field has a default); else `validator`. */
export function optional(validator: Validator): Validator {
  return (text) => (text.trim() === '' ? null : validator(text));
}

/** A value is needed. `what` names it with its article: "a bucket". */
export function required(what = 'a value'): Validator {
  return (text) => (text.trim() === '' ? `Enter ${what}.` : null);
}

/** User and group names: the server's `iam_name_error`. */
export function iamName(): Validator {
  return (text) => iamNameError(text);
}

/** Replication and lifecycle rule names: `[A-Za-z0-9_.-]{1,64}`
 *  (`is_valid_replication_rule_name`, config_sections.rs). */
export function ruleName(): Validator {
  return (text) => {
    if (text === '') return 'Enter a rule name.';
    if (/\s/.test(text)) return 'Remove the spaces. Use letters, digits, dots, dashes and underscores only.';
    if (!/^[A-Za-z0-9_.-]*$/.test(text)) return 'Use letters, digits, dots, dashes and underscores only.';
    if (text.length > 64) return `Use 64 characters or fewer (this name has ${text.length}).`;
    return null;
  };
}

/** Request-rule (admission block) names: 1–128 of `[A-Za-z0-9_:.-]`, not
 *  starting with the reserved `public-prefix:` (admission/spec.rs). */
export function admissionBlockName(): Validator {
  return (text) => {
    if (text === '') return 'Enter a rule name.';
    if (!/^[A-Za-z0-9_:.-]*$/.test(text)) return 'Use letters, digits and _ : . - only.';
    if (text.length > 128) return `Use 128 characters or fewer (this name has ${text.length}).`;
    if (text.startsWith('public-prefix:')) return 'Names that start with public-prefix: are reserved.';
    return null;
  };
}

/** Bucket names: the server's `validate_bucket_name`. */
export function bucketName(): Validator {
  return (text) => (text === '' ? 'Enter a bucket name.' : bucketNameError(text));
}

/** A duration such as 30d, 12h or 90m (humantime); `min` = the server's minimum. */
export function duration(opts: { min?: { seconds: number; text: string } } = {}): Validator {
  return (text) => durationError(text, opts);
}

/**
 * A whole number between `min` and `max`. The text must be digits only
 * (an optional minus sign): "1e3", "12.5" and "abc" are refused, never
 * rounded or clamped.
 */
export function integer({ min, max }: { min?: number; max?: number } = {}): Validator {
  const range =
    min !== undefined && max !== undefined
      ? `between ${fmt(min)} and ${fmt(max)}`
      : min !== undefined
        ? `of ${fmt(min)} or more`
        : max !== undefined
          ? `of ${fmt(max)} or less`
          : '';
  const sentence = `Enter a whole number${range ? ` ${range}` : ''}.`;
  return (text) => {
    const t = text.trim();
    if (!/^-?\d+$/.test(t)) return sentence;
    const n = Number(t);
    if ((min !== undefined && n < min) || (max !== undefined && n > max)) return sentence;
    return null;
  };
}

const SIZE_UNITS: Record<string, number> = {
  '': 1, b: 1,
  kb: 1e3, mb: 1e6, gb: 1e9, tb: 1e12, pb: 1e15,
  kib: 2 ** 10, mib: 2 ** 20, gib: 2 ** 30, tib: 2 ** 40, pib: 2 ** 50,
  k: 2 ** 10, m: 2 ** 20, g: 2 ** 30, t: 2 ** 40,
};

/** Pure: "10 GiB", "512MB", "1.5 TiB", "4096" → bytes; null when not a size. */
export function parseByteSize(text: string): number | null {
  const m = /^\s*(\d+(?:\.\d+)?)\s*([a-zA-Z]*)\s*$/.exec(text);
  if (!m) return null;
  const unit = SIZE_UNITS[m[2].toLowerCase()];
  if (unit === undefined) return null;
  return Math.round(Number(m[1]) * unit);
}

/** A size or a quota: a number with an optional unit (B, KB…PB, KiB…PiB),
 *  above 0, at most `maxBytes` (with its words in `maxText`). */
export function size({ maxBytes, maxText }: { maxBytes?: number; maxText?: string } = {}): Validator {
  return (text) => {
    const bytes = parseByteSize(text);
    if (bytes === null) return 'Enter a size such as 500 MiB, 10 GiB or 2 TiB.';
    if (bytes <= 0) return 'Enter a size above 0.';
    if (maxBytes !== undefined && bytes > maxBytes) return `Enter at most ${maxText ?? `${fmt(maxBytes)} bytes`}.`;
    return null;
  };
}

/** An absolute http(s) URL (an OIDC issuer, a webhook). */
export function url({ httpsOnly = false }: { httpsOnly?: boolean } = {}): Validator {
  return (text) => {
    let parsed: URL;
    try {
      parsed = new URL(text.trim());
    } catch {
      return 'Enter a full URL that starts with https://.';
    }
    if (parsed.protocol !== 'https:' && (httpsOnly || parsed.protocol !== 'http:')) {
      return httpsOnly ? 'Use an https:// URL.' : 'Use an http:// or https:// URL.';
    }
    if (!parsed.hostname) return 'Enter a URL with a host name.';
    return null;
  };
}

/** One IPv4 or IPv6 address. */
export function ip(): Validator {
  return (text) => {
    const t = text.trim();
    return isIpv4(t) || isIpv6(t) ? null : `"${t}" is not an IP address such as 203.0.113.7 or 2001:db8::1.`;
  };
}

/** One IP address or network (CIDR), as the request-rule source IPs. */
export function ipOrCidr(): Validator {
  return (text) => {
    const t = text.trim();
    const why = sourceIpEntryError(t);
    return why ? `"${t}" ${why}.` : null;
  };
}

const HOSTNAME = /^(?=.{1,253}$)([A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?)(\.[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?)*$/;

/** host:port — an IPv4 address, [IPv6] or a host name, then a port 1–65535
 *  (a socket address, as `advanced.listen_addr`). */
export function hostPort(): Validator {
  return (text) => {
    const t = text.trim();
    const hint = 'Enter host:port, such as 0.0.0.0:9000 or [::]:9000.';
    const m = /^(\[([^\]]+)\]|[^:\s]+):(\d{1,5})$/.exec(t);
    if (!m) return hint;
    const host = m[2] ?? m[1];
    const hostOk = m[2] !== undefined ? isIpv6(host) : isIpv4(host) || (!/^[\d.]+$/.test(host) && HOSTNAME.test(host));
    if (!hostOk) return `"${host}" is not an IP address or a host name. ${hint}`;
    const port = Number(m[3]);
    if (port < 1 || port > 65535) return 'Enter a port between 1 and 65,535.';
    return null;
  };
}
