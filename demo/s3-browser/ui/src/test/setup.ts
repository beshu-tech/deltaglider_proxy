// jsdom lacks the layout APIs AntD 6 calls on mount; stub them so components
// render. The stubs report "no match / no size", which is what a test wants.
import '@testing-library/jest-dom/vitest';
import { act, cleanup } from '@testing-library/react';
import { ConfigProvider, Modal, message, notification } from 'antd';
import { createElement } from 'react';
import { NO_MOTION } from './render';
import { afterAll, afterEach, vi } from 'vitest';

// jsdom never fires animation/transition end, so AntD motion would leave
// closed dialogs mounted and fire timers after the test. Static
// Modal.confirm / message render outside the tree: give them the same theme.
ConfigProvider.config({
  holderRender: (children) => createElement(ConfigProvider, { theme: NO_MOTION }, children),
});

afterEach(async () => {
  cleanup();
  // Static Modal.confirm / message / notification render outside the test's
  // root, so cleanup() does not reach them. Close them inside act() and let
  // React finish: work left on the scheduler would run after jsdom is torn
  // down ("window is not defined").
  await act(async () => {
    Modal.destroyAll();
    message.destroy();
    notification.destroy();
    await new Promise((r) => setTimeout(r, 0));
  });
});

// A loaded machine can still hold scheduled React work (AntD motion timers,
// late promise settles) when the last test ends; drain it before jsdom goes.
afterAll(async () => {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 100));
  });
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
