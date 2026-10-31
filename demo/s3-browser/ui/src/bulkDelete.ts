/**
 * The words of a bulk delete: its confirmation and its start message.
 *
 * React-free so the node unit project tests it — see
 * src/__tests__/bulkDelete.test.ts. The browser counts the selection,
 * asks, then starts a server job (useS3Browser.bulkDelete); the bucket's
 * status shows the job (DeleteJobsBanner).
 */
import { count } from './bulkBatches';
import { noun } from './utils';

/** What a delete is about to remove, counted before the confirmation. */
export interface DeletePlanSummary {
  /** Objects to delete: the selected objects plus everything inside the selected folders. */
  objects: number;
  /** Selected folders. */
  folders: number;
  /** Selected objects outside the folders. */
  files: number;
}

/** From this many objects the confirmation explains how a long delete runs. */
const LARGE_DELETE_OBJECTS = 10_000;

/**
 * The words of the delete confirmation, shown after the selection is
 * counted: the exact number of objects, what the selection holds, and for a
 * large delete how it runs and how it stops.
 */
export function bulkDeleteConfirm(s: DeletePlanSummary): { title: string; body: string[]; okText: string } {
  const objects = `${count(s.objects)} ${noun(s.objects, 'object')}`;
  const parts = [
    s.folders > 0 ? `${count(s.folders)} ${noun(s.folders, 'folder')}` : '',
    s.files > 0 ? `${count(s.files)} ${noun(s.files, 'object')}` : '',
  ].filter(Boolean);
  const body =
    s.folders > 0
      ? [`You selected ${parts.join(' and ')}. With everything inside the ${noun(s.folders, 'folder')}, that is ${objects}. This cannot be undone.`]
      : [`This cannot be undone.`];
  if (s.objects >= LARGE_DELETE_OBJECTS) {
    body.push(
      `A delete this large can take several minutes. It runs on the server, so you can close this tab. The bucket shows how far it got.`,
      `Cancel stops it after the page in flight. Objects deleted before that stay deleted.`,
    );
  }
  return {
    title: `Delete ${objects} permanently?`,
    body,
    okText: s.objects >= LARGE_DELETE_OBJECTS ? `Delete ${objects}` : 'Delete',
  };
}

/** How a delete started from the browser ended on the browser's side. */
export interface DeleteJobOutcome {
  /** Objects counted for the delete. */
  total: number;
  /** The user stopped it before it started (at the count or the confirmation). */
  cancelled: boolean;
  /** Always 0: the job reports its failures in the bucket status. */
  failed: number;
  /** The server job that deletes, when one started. */
  jobId?: number;
}

/** The message after a delete was started, or not; null after the user
 *  cancelled the confirmation: the closed dialog says it (UX harvest D19). */
export function deleteJobMessage(o: DeleteJobOutcome): { type: 'info'; text: string } | null {
  if (o.cancelled) return null;
  if (o.total === 0 || o.jobId === undefined) return { type: 'info', text: 'Nothing to delete.' };
  // A started job is progress, not a result: never the success green.
  return {
    type: 'info',
    text: `Deleting ${count(o.total)} ${noun(o.total, 'object')} on the server. You can close this tab: the bucket shows the progress.`,
  };
}
