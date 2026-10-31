/**
 * THE upload outcome model (UX harvest D23–D31).
 *
 * Every file on the upload page is in ONE state:
 *
 *   staged ──► refused                    (the pre-check: size limit, quota)
 *      └────► uploading ──► done | failed | cancelled
 *
 * The staged list, the counts above it, the Upload button label and the
 * final banner all read these functions, so they cannot disagree ("3 files
 * ready" above "Upload 0 files", a green "Upload complete" over two failed
 * rows). React-free: `uploadOutcome.test.ts` covers the truth table.
 */
import { formatBytes } from './utils';
import type { UploadRetryAdvice, UploadStatus } from './uploadTelemetry';

/** The path a file gets under the destination (a folder pick keeps its tree). */
function stagedPath(file: File): string {
  return (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
}

/** One staged file: the same file picked twice is one entry. */
export function stagedId(file: File): string {
  return `${stagedPath(file)}\u0000${file.size}\u0000${file.lastModified}`;
}

/**
 * `prev` plus `added`. The same file again is ignored. A DIFFERENT file with
 * a path that is already staged replaces the old entry in place (both would
 * upload to the same key, and the second would overwrite the first); its
 * path is returned in `replaced` so the list can mark the row.
 */
export function mergeStaged(prev: File[], added: File[]): { files: File[]; replaced: string[] } {
  const next = [...prev];
  const ids = new Set(prev.map(stagedId));
  const indexByPath = new Map(prev.map((f, i) => [stagedPath(f), i]));
  const replaced: string[] = [];
  for (const f of added) {
    if (ids.has(stagedId(f))) continue;
    const path = stagedPath(f);
    const at = indexByPath.get(path);
    if (at !== undefined) {
      ids.delete(stagedId(next[at]));
      next[at] = f;
      if (!replaced.includes(path)) replaced.push(path);
    } else {
      indexByPath.set(path, next.length);
      next.push(f);
    }
    ids.add(stagedId(f));
  }
  return { files: next, replaced };
}

/** A staged file, with the pre-check's verdict on it. */
export interface StagedRow {
  file: File;
  path: string;
  state: 'staged' | 'refused';
  /** Why the server would refuse it (state `refused`). */
  reason?: string;
}

/** Pure: the staged files in list order, each with its state. */
export function stagedRows(files: File[], refused: { file: File; reason: string }[]): StagedRow[] {
  const why = new Map(refused.map((r) => [r.file, r.reason]));
  return files.map((file) => {
    const reason = why.get(file);
    return reason === undefined
      ? { file, path: stagedPath(file), state: 'staged' }
      : { file, path: stagedPath(file), state: 'refused', reason };
  });
}

function files(n: number): string {
  return `${n.toLocaleString()} file${n === 1 ? '' : 's'}`;
}

/**
 * Pure: the line above the staged list. Counts and size are those of the
 * files that CAN be uploaded; refused files are named as refused.
 */
export function stagedHeadline(rows: StagedRow[]): string {
  const ready = rows.filter((r) => r.state === 'staged');
  const bytes = ready.reduce((sum, r) => sum + r.file.size, 0);
  const refused = rows.length - ready.length;
  if (refused === 0) return `${files(ready.length)} ready to upload · ${formatBytes(bytes)}`;
  if (ready.length === 0) return `${files(rows.length)} staged · none can be uploaded`;
  return `${files(rows.length)} staged · ${ready.length.toLocaleString()} can be uploaded (${formatBytes(bytes)}) · ${refused.toLocaleString()} refused`;
}

/**
 * Pure: the Upload button. The destination is named only when it is valid:
 * an invalid path in the label read as if the page accepted it.
 */
export function uploadButtonLabel(ready: number, destination: string | null): string {
  return destination === null ? `Upload ${files(ready)}` : `Upload ${files(ready)} to ${destination}`;
}

/** What the page knows of one queued file. */
export interface QueuedFile {
  status: UploadStatus;
  retry?: UploadRetryAdvice;
}

/** Pure: the state of one queued file in the outcome model. */
export function fileState(status: UploadStatus): 'uploading' | 'done' | 'failed' | 'cancelled' {
  switch (status) {
    case 'success':
      return 'done';
    case 'error':
      return 'failed';
    case 'cancelled':
      return 'cancelled';
    default:
      return 'uploading';
  }
}

/** Pure: can Retry help this failed or cancelled file? (D31) */
export function canRetry(item: QueuedFile): boolean {
  if (item.status === 'cancelled') return true;
  return item.status === 'error' && item.retry === 'retry';
}

export interface QueueOutcome {
  total: number;
  uploading: number;
  done: number;
  failed: number;
  cancelled: number;
  /** Failed files that a retry can fix (transient errors). */
  retryable: number;
  /**
   * `running` while any file is queued or uploading; then `success` only
   * when EVERY file succeeded, `partial` when some did, `failed` when none
   * did and some failed, `cancelled` when the user stopped them all.
   */
  tone: 'idle' | 'running' | 'success' | 'partial' | 'failed' | 'cancelled';
}

/** Pure: the outcome of the upload queue. */
export function queueOutcome(items: QueuedFile[]): QueueOutcome {
  const o = { total: items.length, uploading: 0, done: 0, failed: 0, cancelled: 0, retryable: 0 };
  for (const item of items) {
    const s = fileState(item.status);
    o[s] += 1;
    if (s === 'failed' && canRetry(item)) o.retryable += 1;
  }
  let tone: QueueOutcome['tone'];
  if (o.total === 0) tone = 'idle';
  else if (o.uploading > 0) tone = 'running';
  else if (o.done === o.total) tone = 'success';
  else if (o.done > 0) tone = 'partial';
  else if (o.failed > 0) tone = 'failed';
  else tone = 'cancelled';
  return { ...o, tone };
}

/** Pure: the title and the sentence of the final banner (not while running). */
export function outcomeHeadline(o: QueueOutcome, destination: string): { title: string; body: string } {
  const notDone = [
    o.failed > 0 ? `${o.failed.toLocaleString()} failed` : '',
    o.cancelled > 0 ? `${o.cancelled.toLocaleString()} cancelled` : '',
  ]
    .filter(Boolean)
    .join(', ');
  switch (o.tone) {
    case 'success':
      return { title: 'Upload complete', body: `${files(o.done)} uploaded to ${destination}.` };
    case 'partial':
      return {
        title: `${o.done.toLocaleString()} of ${files(o.total)} uploaded`,
        body: `${notDone}. The list below says why.`,
      };
    case 'failed':
      return { title: 'No files were uploaded', body: `${notDone}. The list below says why.` };
    case 'cancelled':
      return { title: 'Upload cancelled', body: 'No files were uploaded.' };
    default:
      return { title: '', body: '' };
  }
}
