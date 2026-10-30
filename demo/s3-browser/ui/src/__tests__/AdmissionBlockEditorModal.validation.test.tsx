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
