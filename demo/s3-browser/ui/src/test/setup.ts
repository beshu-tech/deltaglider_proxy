// jsdom lacks the layout APIs AntD 6 calls on mount; stub them so components
// render. The stubs report "no match / no size", which is what a test wants.
import '@testing-library/jest-dom/vitest';
import { cleanup } from '@testing-library/react';
import { Modal, message, notification } from 'antd';
import { afterEach, vi } from 'vitest';

afterEach(() => {
  cleanup();
  // Static Modal.confirm / message / notification render outside the test's
  // root, so cleanup() does not reach them: drop them before the next test.
  Modal.destroyAll();
  message.destroy();
  notification.destroy();
  // destroyAll() only starts the close animation; drop the leftover
  // confirm containers so the next test sees one dialog.
  for (const el of document.body.querySelectorAll(':scope > div')) {
    if (el.querySelector('.ant-modal-root, .ant-modal-wrap')) el.remove();
  }
});

if (!window.matchMedia) {
  Object.defineProperty(window, 'matchMedia', {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn(),
    }),
  });
}

class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
if (!('ResizeObserver' in window)) {
  Object.defineProperty(window, 'ResizeObserver', { writable: true, value: ResizeObserverStub });
  Object.defineProperty(globalThis, 'ResizeObserver', { writable: true, value: ResizeObserverStub });
}

// rc-util measures scrollbars through getComputedStyle with a pseudo element,
// which jsdom does not implement; drop the pseudo argument.
const realGetComputedStyle = window.getComputedStyle.bind(window);
window.getComputedStyle = (elt: Element) => realGetComputedStyle(elt);

if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}
