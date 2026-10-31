// C10: the rule editor's name error uses the one copy style of the admin
// forms (a sentence on the field, the field marked invalid), not
// "name is required".
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import AdmissionBlockEditorModal from '../components/AdmissionBlockEditorModal';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  const http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('an empty name is a sentence on the field and nothing is saved', async () => {
  const onSave = vi.fn();
  const user = userEvent.setup();
  renderWithQuery(<AdmissionBlockEditorModal open initial={null} otherNames={[]} onCancel={() => {}} onSave={onSave} />);
  await user.click(screen.getByRole('button', { name: 'Add rule' }));
  const name = screen.getByRole('textbox', { name: 'Name' });
  await vi.waitFor(() => expect(name).toHaveAttribute('aria-invalid', 'true'));
  const errorId = name.getAttribute('aria-describedby')!.split(' ')[0];
  const sentence = document.getElementById(errorId)!.textContent ?? '';
  expect(sentence).toMatch(/^[A-Z].*\.$/);
  expect(sentence).not.toContain('name is required');
  expect(onSave).not.toHaveBeenCalled();
});

// D18, D52: the default action (Deny) opens with plain help, not a yellow warning box.
test('the action explanation is plain help text, not a warning', async () => {
  renderWithQuery(<AdmissionBlockEditorModal open initial={null} otherNames={[]} onCancel={() => {}} onSave={() => {}} />);
  expect(screen.getByTestId('admission-action-help')).toHaveTextContent('403 Access Denied');
  expect(document.querySelector('.ant-alert-warning')).toBeNull();
});

// D9 class: the reject status is not clamped; an out-of-range code is an error and blocks Save.
test('an out-of-range reject status shows an error and nothing is saved', async () => {
  const onSave = vi.fn();
  const user = userEvent.setup();
  renderWithQuery(
    <AdmissionBlockEditorModal
      open
      initial={{ name: 'maint', match: {}, action: { type: 'reject', status: 503, message: '' } }}
      otherNames={[]}
      onCancel={() => {}}
      onSave={onSave}
    />,
  );
  const status = screen.getByRole('textbox', { name: 'Status code' });
  await user.clear(status);
  await user.type(status, '99');
  await user.tab();
  expect(status).toHaveValue('99');
  expect(status).toHaveAttribute('aria-invalid', 'true');
  await user.click(screen.getByRole('button', { name: 'Save' }));
  expect(onSave).not.toHaveBeenCalled();
});
