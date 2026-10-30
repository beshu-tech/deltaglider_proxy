/**
 * An admin JSON error body. The admin error type always sends
 * `{error: <code>, message: <text>}`. The config write surfaces
 * (`{ok|success|applied: false, error}`) and the backup import send the
 * text in `error` and no `message`.
 */
type JsonErrorBody = { error?: unknown; message?: unknown };

/** Pure: the code and the text of an admin JSON error body. */
export function parseErrorBody(data: unknown): { code?: string; message?: string } {
  if (!data || typeof data !== 'object') return {};
  const { error, message } = data as JsonErrorBody;
  const err = typeof error === 'string' && error ? error : undefined;
  const msg = typeof message === 'string' && message ? message : undefined;
  if (msg) return { code: err, message: msg };
  return { message: err };
}

function statusHint(status: number): string {
  switch (status) {
    case 400:
      return 'Bad request. Verify request shape and parameters.';
    case 401:
      return 'Authentication required or session expired.';
    case 403:
      return 'Access denied. Credentials are valid but permissions do not allow this operation.';
    case 404:
      return 'Requested resource was not found.';
    case 409:
      return 'Conflict with current resource state.';
    case 413:
      return 'Payload too large. This usually means the object exceeds proxy/reverse-proxy upload limits (for DeltaGlider: DGP_MAX_OBJECT_SIZE).';
    case 429:
      return 'Rate limited. Retry after a short delay.';
    case 500:
      return 'Server internal error.';
    case 502:
      return 'Gateway returned 502 Bad Gateway. Upstream/proxy dropped the request (common on long uploads). Retry, and verify reverse-proxy timeout/body-size limits.';
    case 503:
      return 'Service unavailable (503). Upstream/gateway could not process the request (often transient during long uploads). Retry, and verify gateway/proxy limits and health.';
    case 504:
      return 'Gateway timeout (504). Upload exceeded upstream timeout. Retry, and increase gateway/proxy timeouts for large uploads.';
    default:
      return '';
  }
}

/** ` (request-id: …)` suffix, or empty string when no request id is present. */
export function requestIdSuffix(requestId: string | undefined | null): string {
  return requestId ? ` (request-id: ${requestId})` : '';
}

function trimOneLine(text: string, max = 280): string {
  const compact = text.replace(/\s+/g, ' ').trim();
  if (compact.length <= max) return compact;
  return `${compact.slice(0, max - 1)}…`;
}

function parseXmlError(xmlText: string): { code?: string; message?: string } {
  try {
    const doc = new DOMParser().parseFromString(xmlText, 'application/xml');
    const code = doc.querySelector('Error > Code')?.textContent?.trim();
    const message = doc.querySelector('Error > Message')?.textContent?.trim();
    return { code: code || undefined, message: message || undefined };
  } catch {
    return {};
  }
}

async function readResponseBodyMessage(res: Response): Promise<{ code?: string; message?: string }> {
  const ct = (res.headers.get('content-type') || '').toLowerCase();
  const body = await res.text();
  if (!body) return {};

  if (ct.includes('application/json')) {
    try {
      const parsed = parseErrorBody(JSON.parse(body));
      return { code: parsed.code, message: parsed.message || trimOneLine(body) };
    } catch {
      return { message: trimOneLine(body) };
    }
  }

  if (ct.includes('xml') || body.includes('<Error>')) {
    const parsed = parseXmlError(body);
    return {
      code: parsed.code,
      message: parsed.message || trimOneLine(body),
    };
  }

  return { message: trimOneLine(body) };
}

/**
 * An HTTP failure from the proxy — admin API or S3 (see `normalizeS3Error`).
 * The message keeps its operator-facing shape; `status`, `code` and `detail` carry the facts that code decides on, so no caller
 * has to parse the message text.
 */
export class ApiError extends Error {
  readonly status: number;
  readonly code?: string;
  /** The server's error text (JSON `message`, or a lone `error`), when it sent one. */
  readonly detail?: string;

  constructor(message: string, status: number, code?: string, detail?: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.detail = detail;
  }
}

export async function throwApiError(res: Response, context: string): Promise<never> {
  const requestId = res.headers.get('x-amz-request-id') || res.headers.get('x-request-id') || '';
  const { code, message } = await readResponseBodyMessage(res.clone());
  const hint = statusHint(res.status);
  const main = message || hint || `HTTP ${res.status}`;
  const codePart = code ? ` [${code}]` : '';
  const reqPart = requestIdSuffix(requestId);
  throw new ApiError(
    `${context} failed (${res.status})${codePart}: ${main}${reqPart}`,
    res.status,
    code,
    message,
  );
}

/**
 * THE rule for "send the user back to sign-in": a 401, or a 403 whose body is
 * `admin_session_required` (a browser-only session on an admin route).
 * Decided on the status and the error code, never on message text.
 */
export function isSessionExpired(err: unknown): boolean {
  if (!(err instanceof ApiError)) return false;
  return err.status === 401 || (err.status === 403 && err.code === 'admin_session_required');
}

function stringifyUnknown(err: unknown): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

type S3LikeError = {
  name?: string;
  message?: string;
  Code?: string;
  code?: string;
  $metadata?: {
    httpStatusCode?: number;
    requestId?: string;
  };
};

/**
 * A failed S3 request as an `ApiError`: the message is the operator-facing
 * text; `status` (0 = no HTTP answer, as in XHR) and `code` carry the facts
 * code decides on, so no caller parses the message.
 */
export function normalizeS3Error(err: unknown, context: string): ApiError {
  const e = err as S3LikeError;
  const prior = err instanceof ApiError ? err : undefined;
  return new ApiError(
    describeS3Error(err, context).message,
    prior?.status ?? e?.$metadata?.httpStatusCode ?? 0,
    prior?.code ?? (e?.Code || e?.code || e?.name),
    prior?.detail ?? e?.message,
  );
}

/** Error names that the AWS SDK gives a failure whose body it could not read
 *  as an S3 XML error (empty, HTML from a gateway, plain text). */
const UNPARSED_ERROR_NAMES = new Set(['Unknown', 'UnknownError', 'Error', 'TypeError']);

/**
 * The `<Message>` of an S3 XML error body, or null when the body was not
 * one. The SDK names a parsed error by its `<Code>` and sets the message to
 * `<Message>`; a body it could not parse gets the name `Unknown` and the
 * message `UnknownError` (empty body), or a parse error (HTML, text).
 */
function s3XmlErrorMessage(e: S3LikeError | undefined): string | null {
  const code = e?.Code || e?.code || e?.name;
  const message = e?.message?.trim();
  if (!code || !message || UNPARSED_ERROR_NAMES.has(code) || /^\d+$/.test(code)) return null;
  if (message === 'UnknownError' || message === code) return null;
  if (/DOMParser|XML parsing error|char 0|deserialization/i.test(message)) return null;
  return message;
}

function describeS3Error(err: unknown, context: string): Error {
  const e = err as S3LikeError;
  const raw = stringifyUnknown(err);
  const status = e?.$metadata?.httpStatusCode;
  const requestId = e?.$metadata?.requestId;
  const code = e?.Code || e?.code || e?.name;
  const hasDomParserNoise = /DOMParser|XML parsing error|char 0|deserialization/i.test(raw);
  const hasGatewayBody = /(^|[\s:])bad gateway($|[\s.])|service unavailable|gateway timeout/i.test(raw);

  // Browser AWS SDK often throws XML parser noise when server/proxy returned non-XML.
  // Convert this into actionable output with status-based hints.
  if (hasDomParserNoise) {
    const hint = status ? statusHint(status) : 'Server returned a non-XML error response.';
    const reqPart = requestIdSuffix(requestId);
    return new Error(`${context} failed${status ? ` (${status})` : ''}: ${hint}${reqPart}`);
  }

  // The proxy's own 5xx answers (backend health, maintenance, rate limit,
  // concurrency cap, spool budget, codec slots, multipart caps, an object
  // that changed while it was read, the encryption witness, an admission
  // `reject`) are S3 XML errors with a crafted <Message>. Surface it
  // VERBATIM: the gateway hint would send the operator to reverse-proxy
  // limits. The hint stays for bodies that are not ours (see below).
  const serverMessage = s3XmlErrorMessage(e);
  if (status !== undefined && status >= 500 && serverMessage) {
    const reqPart = requestIdSuffix(requestId);
    return new Error(`${context} failed (${status}) [${code}]: ${serverMessage}${reqPart}`);
  }

  // Gateway/plaintext failures during large PUTs may arrive as non-S3 bodies.
  // Make these explicit even when SDK metadata is incomplete.
  if (status === 502 || status === 503 || status === 504 || hasGatewayBody) {
    const effectiveStatus = status ?? (/\b503\b|service unavailable/i.test(raw) ? 503 : /\b504\b|gateway timeout/i.test(raw) ? 504 : 502);
    const hint = statusHint(effectiveStatus);
    const reqPart = requestIdSuffix(requestId);
    return new Error(`${context} failed (${effectiveStatus}): ${hint}${reqPart}`);
  }

  if (status === 413 || code === 'EntityTooLarge') {
    const reqPart = requestIdSuffix(requestId);
    return new Error(
      `${context} failed (${status ?? 413}) [EntityTooLarge]: ${statusHint(413)}${reqPart}`,
    );
  }

  if (status === 403 && /SignatureDoesNotMatch|AccessDenied/i.test(raw)) {
    const reqPart = requestIdSuffix(requestId);
    return new Error(
      `${context} failed (403): Access denied or signature mismatch. Reconnect with valid credentials/permissions.${reqPart}`,
    );
  }

  const codePart = code ? ` [${code}]` : '';
  const statusPart = status ? ` (${status})` : '';
  const reqPart = requestIdSuffix(requestId);
  return new Error(`${context} failed${statusPart}${codePart}: ${trimOneLine(raw)}${reqPart}`);
}

export function normalizeUiError(err: unknown, fallback: string): string {
  if (err instanceof Error && err.message) return err.message;
  if (typeof err === 'string' && err.trim()) return err.trim();
  return fallback;
}
