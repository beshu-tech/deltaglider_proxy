/**
 * UX harvest D4: in the Create-bucket modal, Tab on "Create" (the last
 * control) put focus on BODY: AntD's focus lock has no sentinels and the
 * modal is the last thing in <body>, so the browser's default Tab leaves
 * the document. The wrap moves focus to the first control instead.
 */
import { fireEvent, render, screen } from '@testing-library/react';
import { Button, Input, Modal } from 'antd';
import { afterEach, beforeEach, expect, test } from 'vitest';
import { installDialogTabWrap, topmostModalDialog } from '../dialogTabWrap';

let uninstall: () => void = () => {};
beforeEach(() => {
  uninstall = installDialogTabWrap(document);
});
afterEach(() => uninstall());

function renderModal(open = true, mask = true) {
  return render(
    <>
      <button type="button">Page button</button>
      <Modal open={open} mask={mask} title="Create bucket" footer={<Button>Create</Button>} getContainer={document.body}>
        <Input aria-label="Bucket name" />
      </Modal>
    </>,
  );
}

test('Tab on the last control of a modal goes to the first, not out of the page', () => {
  renderModal();
  const create = screen.getByRole('button', { name: 'Create' });
  create.focus();
  const ev = fireEvent.keyDown(create, { key: 'Tab' });
  expect(ev).toBe(false); // default prevented: the browser does not move focus out
  expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Close' }));
});

test('Shift+Tab on the first control goes to the last', () => {
  renderModal();
  const close = screen.getByRole('button', { name: 'Close' });
  close.focus();
  fireEvent.keyDown(close, { key: 'Tab', shiftKey: true });
  expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Create' }));
});

test('Tab in the middle of the modal is left to the browser', () => {
  renderModal();
  const input = screen.getByLabelText('Bucket name');
  input.focus();
  expect(fireEvent.keyDown(input, { key: 'Tab' })).toBe(true);
});

test('no open modal: Tab is never touched', () => {
  renderModal(false);
  expect(topmostModalDialog(document)).toBeNull();
  const page = screen.getByRole('button', { name: 'Page button' });
  page.focus();
  expect(fireEvent.keyDown(page, { key: 'Tab' })).toBe(true);
});
