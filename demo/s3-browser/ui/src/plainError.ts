/**
 * THE error-to-words layer (UX harvest C2, D17, D24).
 *
 * Every failure the UI shows passes through `plainError`: an `ApiError` of
 * an admin or an S3 request, a raw AWS SDK error, a fetch `TypeError`, an
 * `Error`, or the operator-facing string that `describeS3Error` builds
 * ("Upload object x failed (403) [AccessDenied]: … (request-id: …)").
 *
 * The result is ONE plain sentence a person can act on (`text`), and the raw
 * wire text (`details`) for a "Details" disclosure: the status code, the
 * error code, `[internal]`, the request id and the server text live ONLY in
 * `details`. Render it with <ErrorState> or <PlainErrorAlert>, never as
 * `err.message` (source test `noRawErrorText.test.ts`).
 *
 * Pure: no React, no fetch, so the whole truth table is unit-tested
 * (`plainError.test.ts`).
 */
import { ApiError } from './errorHandling';

/** An error as the UI shows it: one plain sentence, and the raw text for a
 *  "Details" disclosure when the sentence leaves something out. */
export interface PlainError {
  text: string;
  details?: string;
}

/** Pure: a duration parser message ("expected number at 0", humantime). */
const DURATION_PARSE_ERROR = /expected number at \d+|unknown time unit|invalid character at \d+|value was empty|number is too large/i;

/** Pure: the plain sentence for the cause of a failed backend probe
 *  (`HealthVerdict::cause`: "<kind> (<detail>) — <advice>"). */
function probeCauseSentence(kind: string, detail: string): string {
  // A filesystem probe reports "<path>: <io error> (os error N)".
  const io = /^(\/.*?): (.+?)(?: \(os error \d+\))?$/.exec(detail);
  if (io) {
    const [, path, cause] = io;
    if (/permission denied/i.test(cause)) return `The proxy cannot write to ${path} (permission denied).`;
    if (/read-only file system/i.test(cause)) return `The proxy cannot write to ${path}: the file system is read-only.`;
    if (/not a directory/i.test(cause)) return `${path} is not a directory, or a part of the path is a file.`;
    if (/no space left/i.test(cause)) return `The disk of ${path} is full.`;
    return `The proxy cannot use ${path}: ${cause.charAt(0).toLowerCase()}${cause.slice(1)}.`;
  }
  if (kind === 'credentials rejected') return 'The backend refuses the access key or the secret key.';
  if (kind === 'backend erroring') return 'The backend answers, but with server errors.';
  if (/dns|lookup address|name or service not known|no such host/i.test(detail)) {
    return 'The proxy cannot find the endpoint host name (DNS).';
  }
  if (/connection refused/i.test(detail)) return 'The endpoint refuses the connection.';
  if (/timed? ?out/i.test(detail)) return 'The endpoint does not answer in time.';
  if (/certificate|tls|ssl/i.test(detail)) return 'The TLS connection to the endpoint fails (certificate).';
  return 'The proxy cannot reach the endpoint.';
}

/** Pure: the plain sentence for a backend health verdict that is not
 *  healthy (`BackendHealthEntry`: the badge title and "Test connection"). */
export function backendHealthSentence(status: 'auth-rejected' | 'unreachable' | 'erroring', detail: string): string {
  const kind = status === 'auth-rejected' ? 'credentials rejected' : status === 'erroring' ? 'backend erroring' : 'endpoint unreachable';
  return probeCauseSentence(kind, detail);
}

/** Pure: one plain sentence for a server error text, or null when the
 *  text has no known shape (the caller then shows it as it is). */
function knownErrorSentence(text: string): string | null {
  const probe = /failed its connection probe — (credentials rejected|endpoint unreachable|backend erroring) \((.*)\) — /.exec(text);
  if (probe) {
    const unchanged = /nothing was changed/i.test(text) ? ' Nothing was changed.' : '';
    return `${probeCauseSentence(probe[1], probe[2])}${unchanged}`;
  }
  // A config validation error about a duration: "<what> <field>=<value> invalid: <parser message>".
  const duration = /([\w.]+)=(\S*) invalid: (.*)$/.exec(text);
  if (duration && DURATION_PARSE_ERROR.test(duration[3])) {
    const field = duration[1].replace(/_/g, ' ');
    const where = /rule '([^']+)'/.exec(text);
    return `${duration[2] ? `"${duration[2]}"` : 'An empty value'} is not a valid duration for ${field}${where ? ` of rule ${where[1]}` : ''}. Use a duration such as 30d, 12h or 90m.`;
  }
  return null;
}

/** Pure: a sentence starts with a capital and ends with a full stop. */
function asSentence(text: string): string {
  const t = text.replace(/\s*\(os error \d+\)/g, '').trim();
  if (!t) return t;
  // An identifier ("cache_size_mb", "storage.buckets") keeps its case.
  const word = t.split(/\s/, 1)[0];
  const first = /^[a-z][a-z']*[,:]?$/.test(word) ? t.charAt(0).toUpperCase() + t.slice(1) : t;
  return /[.!?]$/.test(first) ? first : `${first}.`;
}


/** The facts of a failed request, from any error shape. */
interface ErrorFacts {
  /** HTTP status; 0 = no HTTP answer (network), undefined = not known. */
  status?: number;
  code?: string;
  /** The server's own text (JSON `message`, S3 `<Message>`). */
  detail?: string;
  /** The full operator-facing text, for `details`. */
  raw: string;
}

type SdkLikeError = {
  name?: string;
  message?: string;
  Code?: string;
  code?: string;
  $metadata?: { httpStatusCode?: number; requestId?: string };
};

/** "<context> failed (<status>) [<code>]: <text> (request-id: <id>)" — the
 *  operator-facing shape of `throwApiError` and `describeS3Error`. */
const WIRE_SHAPE = /^.*? failed(?: \((\d{3})\))?(?: \[([\w.:-]+)\])?: (.*?)(?: \(request-id: [^)]*\))?$/s;

/** Pure: the facts of any error value. */
function errorFacts(err: unknown): ErrorFacts {
  if (err instanceof ApiError) {
    const wire = WIRE_SHAPE.exec(err.message);
    return { status: err.status, code: err.code, detail: err.detail ?? wire?.[3], raw: err.message };
  }
  const sdk = (err && typeof err === 'object' ? err : {}) as SdkLikeError;
  if (sdk.$metadata) {
    const code = sdk.Code || sdk.code || sdk.name;
    const id = sdk.$metadata.requestId;
    const status = sdk.$metadata.httpStatusCode;
    const raw = `${status ? `(${status}) ` : ''}${code ? `[${code}] ` : ''}${sdk.message ?? ''}${id ? ` (request-id: ${id})` : ''}`.trim();
    return { status, code, detail: sdk.message, raw };
  }
  const text = err instanceof Error ? err.message : typeof err === 'string' ? err : '';
  if (err instanceof TypeError && /failed to fetch|networkerror|load failed|network request failed/i.test(text)) {
    return { status: 0, raw: text };
  }
  const bare = /^HTTP (\d{3})$/.exec(text.trim());
  if (bare) return { status: Number(bare[1]), raw: text };
  const wire = WIRE_SHAPE.exec(text);
  if (wire && (wire[1] || wire[2])) {
    return { status: wire[1] ? Number(wire[1]) : undefined, code: wire[2], detail: wire[3], raw: text };
  }
  return { raw: text };
}

/** Pure: a generic SDK/statusHint text that says nothing the sentence does not. */
const GENERIC_DETAIL =
  /^(UnknownError|Unknown|Server internal error\.?|internal|Access denied.*|Bad request.*|Requested resource was not found\.?|Rate limited.*|Payload too large.*|Gateway .*|Service unavailable.*|Please reduce your request rate\.?|HTTP \d+)$/i;

/** The sentence of an HTTP status that says it all, whatever the body. */
const STATUS_SENTENCE: Record<number, string> = {
  401: 'Your session ended. Sign in again.',
  413: 'The request is larger than the proxy accepts.',
  429: 'Too many requests. Wait a moment and try again.',
};

/** Pure: the plain sentence for an S3 (or HTTP) failure, or null. */
function knownRequestSentence({ status, code, detail }: ErrorFacts): string | null {
  const quota = detail && /Bucket quota exceeded: (.+?) used \+ (.+?) upload > (.+?) limit/.exec(detail);
  if (quota) {
    return `This bucket is full: ${quota[1]} is used of ${quota[3]}, and this file needs ${quota[2]}. Delete objects or ask an administrator to raise the quota.`;
  }
  if (status === 0) return 'The proxy does not answer. Check the network connection and try again.';
  switch (code) {
    case 'SignatureDoesNotMatch':
    case 'InvalidAccessKeyId':
      return 'The access key or the secret key is wrong. Sign in again with valid credentials.';
    case 'AccessDenied':
      return 'You do not have permission to do this. Ask an administrator for access.';
    case 'EntityTooLarge':
      return 'The file is larger than the proxy accepts. Upload a smaller file, or ask an administrator to raise the size limit.';
    case 'NoSuchBucket':
      return 'The bucket does not exist.';
    case 'NoSuchKey':
      return 'The object does not exist. It was possibly deleted or moved.';
    case 'NoSuchUpload':
      return 'The upload is no longer known to the proxy. Start the upload again.';
    case 'PreconditionFailed':
      return 'The object changed while you worked on it. Reload and try again.';
    case 'SlowDown':
    case 'ServiceUnavailable':
      if (detail && !GENERIC_DETAIL.test(detail)) return null; // the proxy's own reason is better
      return 'The proxy is busy. Wait a moment and try again.';
    default:
      break;
  }
  const byStatus = status !== undefined ? STATUS_SENTENCE[status] : undefined;
  if (byStatus) return byStatus;
  if (status === 403 && (!detail || GENERIC_DETAIL.test(detail))) {
    return 'You do not have permission to do this. Ask an administrator for access.';
  }
  if (status === 502 || status === 504) {
    return 'A gateway between you and the proxy dropped the request. Try again; if it fails again, check the reverse-proxy time and size limits.';
  }
  return null;
}

/**
 * THE error-to-words rule: any failure becomes ONE plain sentence, without
 * the request path, the HTTP status, the error code, the request id or an
 * OS error number. Known shapes (a failed backend probe, a bad duration, a
 * full bucket, a denied or throttled S3 request, no network) get their own
 * sentence; otherwise the server's own text is the sentence, and with no
 * server text `fallback` is. The raw text goes in `details` whenever the
 * sentence leaves something out.
 */
export function plainError(err: unknown, fallback: string): PlainError {
  const facts = errorFacts(err);
  const raw = facts.raw || fallback;
  const withDetails = (text: string): PlainError => (text === raw ? { text } : { text, details: raw });
  const serverText = facts.detail ?? (facts.status === undefined && !facts.code ? facts.raw : undefined);
  if (serverText) {
    const known = knownErrorSentence(serverText);
    if (known) return withDetails(known);
  }
  const request = knownRequestSentence(facts);
  if (request) return withDetails(request);
  // An internal server error's text is for the log, not for the user.
  const internal = facts.code === 'internal' || facts.code === 'InternalError';
  if (facts.detail && !internal && !GENERIC_DETAIL.test(facts.detail)) return withDetails(asSentence(facts.detail));
  if (facts.status !== undefined || facts.code) return withDetails(asSentence(fallback));
  return { text: asSentence(raw) };
}
