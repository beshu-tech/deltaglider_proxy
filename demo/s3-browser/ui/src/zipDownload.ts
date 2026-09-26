/**
 * Bulk ZIP download (`GET /_/api/admin/objects/zip?keys=…`).
 *
 * The old flow clicked an `<a download>` link. A failed request (the 413 above
 * the 500 MB cap, a 400, a 5xx) then only cancelled the download: the page
 * showed nothing. The design here:
 *
 *  1. Preflight what the browser knows for free: the key count and the URL
 *     length. The server rejects more than 10,000 keys, and a request line
 *     above 64 KiB never reaches it.
 *  2. Where the browser has the File System Access API (Chrome, Edge), ask
 *     for the save location, then `fetch` the ZIP. A non-2xx answer becomes an
 *     error with the server's message. A 2xx body is piped straight into the
 *     file: it streams to disk and is never held in memory.
 *  3. Elsewhere (Firefox, Safari) keep the streaming `<a download>` link and
 *     say where a failure shows up. Buffering up to 500 MB in memory only to
 *     read the status is not worth it.
 *
 * The server builds the whole archive before it sends the first byte, so a
 * fetch that waits for the status costs nothing extra.
 */
import { fetchWithRelogin } from './adminApi/core';
import { throwApiError } from './errorHandling';
import { formatBytes } from './utils';

/** Mirrors `MAX_BULK_OBJECTS` in src/api/admin/objects.rs. */
const ZIP_MAX_KEYS = 10_000;
/** Mirrors `MAX_ZIP_BYTES` in src/api/admin/objects.rs. */
export const ZIP_MAX_BYTES = 500 * 1024 * 1024;
/** The HTTP stack refuses request targets of 64 KiB and more; keep a margin. */
const ZIP_MAX_URL_LENGTH = 60_000;

/** Why the ZIP cannot be requested at all, or null when it can. */
export function zipPreflightError(keyCount: number, urlLength: number): string | null {
  if (keyCount === 0) return 'The selection has no files to put in a ZIP.';
  if (keyCount > ZIP_MAX_KEYS) {
    return `The selection has ${keyCount.toLocaleString('en-US')} files. One ZIP can hold at most ${ZIP_MAX_KEYS.toLocaleString('en-US')}. Select fewer files.`;
  }
  if (urlLength > ZIP_MAX_URL_LENGTH) {
    return `The selection has too many files (${keyCount.toLocaleString('en-US')}) or file names that are too long for one ZIP request. Select fewer files.`;
  }
  return null;
}

type SaveFilePicker = (opts: {
  suggestedName?: string;
  types?: { description: string; accept: Record<string, string[]> }[];
}) => Promise<{
  createWritable: () => Promise<WritableStream<Uint8Array>>;
  getFile?: () => Promise<{ size: number }>;
  /** Deletes the file (Chrome 110+). */
  remove?: () => Promise<void>;
}>;

/** Injected in tests; the browser globals otherwise. */
export interface ZipDownloadDeps {
  fetch: typeof fetch;
  picker?: SaveFilePicker;
}

function browserDeps(): ZipDownloadDeps {
  return {
    // An expired session asks to sign in again, like every admin request.
    fetch: (input, init) => fetchWithRelogin(String(input), init ?? {}),
    picker: (window as unknown as { showSaveFilePicker?: SaveFilePicker }).showSaveFilePicker,
  };
}

export type ZipOutcome = 'saved' | 'cancelled' | 'started';

function startAnchorDownload(url: string, filename: string): ZipOutcome {
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  return 'started';
}

/**
 * Download the ZIP at `url`. Resolves 'saved' (streamed into the file the
 * user picked), 'cancelled' (the user closed the save dialog) or 'started'
 * (handed to the browser's own download manager). Throws with the server's
 * message when the request fails (an `ApiError`, so a 401 reads as an
 * expired session through `isSessionExpired`).
 */
export async function downloadZip(
  url: string,
  filename: string,
  deps: ZipDownloadDeps = browserDeps(),
): Promise<ZipOutcome> {
  const { picker } = deps;
  if (typeof picker !== 'function') return startAnchorDownload(url, filename);

  let handle: Awaited<ReturnType<SaveFilePicker>>;
  try {
    handle = await picker({
      suggestedName: filename,
      types: [{ description: 'ZIP archive', accept: { 'application/zip': ['.zip'] } }],
    });
  } catch (e) {
    if (e instanceof DOMException && e.name === 'AbortError') return 'cancelled';
    // SecurityError: the click's user activation expired while the folder
    // selection was resolved. The plain link still works.
    return startAnchorDownload(url, filename);
  }

  // Choosing a NEW name makes the picker create an empty file; choosing an
  // existing file leaves its content alone until a writable is committed.
  // So nothing is written before the archive arrives, and on failure only a
  // file that was empty before the request is deleted: an existing file the
  // user picked to overwrite keeps its content.
  const sizeBefore = await handle.getFile?.().then((f) => f.size).catch(() => undefined);
  const discard = async () => {
    if (sizeBefore !== 0) return;
    try {
      await handle.remove?.();
    } catch {
      /* the empty file stays; the error below still explains why */
    }
  };
  let res: Response;
  try {
    res = await deps.fetch(url, { credentials: 'same-origin' });
  } catch (e) {
    await discard();
    throw e;
  }
  if (res.status === 413) {
    await discard();
    throw new Error(
      `The selected files add up to more than ${formatBytes(ZIP_MAX_BYTES)}, the limit for one ZIP. Select fewer files.`,
    );
  }
  if (!res.ok) {
    await discard();
    await throwApiError(res, 'ZIP download');
  }
  if (!res.body) {
    await discard();
    throw new Error('ZIP download failed: the server sent no data.');
  }
  const writable = await handle.createWritable();
  // pipeTo closes the writable on success and aborts it when the connection
  // breaks: an aborted writable discards what it wrote and leaves the file's
  // previous content.
  await res.body.pipeTo(writable);
  return 'saved';
}
