import assert from 'node:assert/strict';
import { test } from 'vitest';
import { bulkDeleteConfirm, deleteJobMessage } from '../bulkDelete';

// A bulk delete counts the selection in the browser, asks with the exact
// number, then starts a server job (it survives closing the tab). These are
// the words of the confirmation and of the start.

test('the message after the start, a cancel, or an empty selection', () => {
  assert.deepEqual(deleteJobMessage({ total: 12_346, cancelled: false, failed: 0, jobId: 7 }), {
    type: 'info',
    text: 'Deleting 12,346 objects on the server. You can close this tab: the bucket shows the progress.',
  });
  // A cancelled confirmation shows no toast (UX harvest D19).
  assert.equal(deleteJobMessage({ total: 0, cancelled: true, failed: 0 }), null);
  assert.deepEqual(deleteJobMessage({ total: 0, cancelled: false, failed: 0 }), {
    type: 'info',
    text: 'Nothing to delete.',
  });
});

test('the delete confirmation gives the exact count, and explains a large delete', () => {
  assert.deepEqual(bulkDeleteConfirm({ objects: 2, folders: 0, files: 2 }), {
    title: 'Delete 2 objects permanently?',
    body: ['This cannot be undone.'],
    okText: 'Delete',
  });
  assert.deepEqual(bulkDeleteConfirm({ objects: 1_201, folders: 1, files: 1 }), {
    title: 'Delete 1,201 objects permanently?',
    body: ['You selected 1 folder and 1 object. With everything inside the folder, that is 1,201 objects. This cannot be undone.'],
    okText: 'Delete',
  });
  const large = bulkDeleteConfirm({ objects: 230_412, folders: 2, files: 0 });
  assert.equal(large.title, 'Delete 230,412 objects permanently?');
  assert.equal(large.body[0], 'You selected 2 folders. With everything inside the folders, that is 230,412 objects. This cannot be undone.');
  assert.match(large.body[1], /It runs on the server, so you can close this tab\./);
  assert.equal(large.body[2], 'Cancel stops it after the page in flight. Objects deleted before that stay deleted.');
  assert.equal(large.okText, 'Delete 230,412 objects', 'a large delete names the count on its button');
});
