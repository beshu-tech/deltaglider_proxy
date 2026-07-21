type JsonErrorShape = {
  error?: string;
  message?: string;
  code?: string;
  details?: string;
};

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
      const data = JSON.parse(body) as JsonErrorShape;
      return {
        code: data.code,
        message: data.error || data.message || data.details || trimOneLine(body),
      };
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

export async function throwApiError(res: Response, context: string): Promise<never> {
  const requestId = res.headers.get('x-amz-request-id') || res.headers.get('x-request-id') || '';
  const { code, message } = await readResponseBodyMessage(res.clone());
  const hint = statusHint(res.status);
  const main = message || hint || `HTTP ${res.status}`;
  const codePart = code ? ` [${code}]` : '';
  const reqPart = requestIdSuffix(requestId);
  throw new Error(`${context} failed (${res.status})${codePart}: ${main}${reqPart}`);
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

export function normalizeS3Error(err: unknown, context: string): Error {
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

  // The proxy's own 503s (backend-health gate, maintenance gate) carry a
  // crafted, actionable <Message> naming the backend and cause — surface it
  // VERBATIM instead of the generic gateway hint, which would misdirect the
  // operator toward reverse-proxy limits.
  const serverMessage = (err as { message?: string })?.message;
  if (
    status === 503 &&
    typeof serverMessage === 'string' &&
    /backend|read-only|unavailable/i.test(serverMessage)
  ) {
    const reqPart = requestIdSuffix(requestId);
    return new Error(`${context} failed (503): ${serverMessage}${reqPart}`);
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
