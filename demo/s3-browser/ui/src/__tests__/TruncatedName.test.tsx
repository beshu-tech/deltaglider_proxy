import { render } from '@testing-library/react';
import { describe, expect, test } from 'vitest';
import TruncatedName from '../components/TruncatedName';
import { splitForMiddleEllipsis } from '../nameTruncation';

describe('splitForMiddleEllipsis', () => {
  test('a short name is never split', () => {
    expect(splitForMiddleEllipsis('releases')).toEqual({ head: 'releases', tail: '' });
  });
  test('two names that differ at the end keep distinct tails', () => {
    const a = splitForMiddleEllipsis('qa-repl-rule-with-a-fairly-long-name-to-dr');
    const b = splitForMiddleEllipsis('qa-repl-rule-with-a-fairly-long-name-to-eu');
    expect(a.tail).not.toBe(b.tail);
    expect(a.head + a.tail).toBe('qa-repl-rule-with-a-fairly-long-name-to-dr');
  });
  test('a medium name keeps one third at the end', () => {
    expect(splitForMiddleEllipsis('db-archive-prod-eu').tail).toBe('rod-eu');
  });
  test('the tail is at most tailChars, counted in code points', () => {
    const name = `${'a'.repeat(60)}-linux-amd64.tar.gz`;
    expect(splitForMiddleEllipsis(name).tail).toBe('a-linux-amd64.tar.gz');
    expect(splitForMiddleEllipsis('😀'.repeat(30), 4).tail).toBe('😀'.repeat(4));
  });
});

test('TruncatedName renders the full name as text and as the hover title', () => {
  const name = 'qa-this-is-a-very-long-bucket-name-to-test-overflow-in-ui-12345';
  const { container } = render(<TruncatedName text={name} />);
  const root = container.firstElementChild as HTMLElement;
  expect(root.textContent).toBe(name);
  expect(root.title).toBe(name);
});

test('title={false} leaves the hover text to an ancestor', () => {
  const { container } = render(<TruncatedName text="releases" title={false} />);
  expect((container.firstElementChild as HTMLElement).hasAttribute('title')).toBe(false);
});

test('a name that fits stays one text node, so it can be found by its text', () => {
  // jsdom has no layout: nothing overflows.
  const { getByText } = render(<TruncatedName text="expire-nightlies-for-the-release-bucket" />);
  expect(getByText('expire-nightlies-for-the-release-bucket')).toBeTruthy();
});

test('an overflowing name is split: the end stays in its own element', () => {
  const name = 'qa-this-is-a-very-long-bucket-name-to-test-overflow-in-ui-12345';
  const sw = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'scrollWidth');
  const cw = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'clientWidth');
  Object.defineProperty(HTMLElement.prototype, 'scrollWidth', { configurable: true, get: () => 500 });
  Object.defineProperty(HTMLElement.prototype, 'clientWidth', { configurable: true, get: () => 100 });
  try {
    const { container } = render(<TruncatedName text={name} />);
    const root = container.firstElementChild as HTMLElement;
    expect(root.children).toHaveLength(2);
    expect(root.children[1].textContent).toBe(splitForMiddleEllipsis(name).tail);
    expect(root.textContent).toBe(name);
  } finally {
    // jsdom defines both on Element.prototype: drop the overrides.
    if (sw) Object.defineProperty(HTMLElement.prototype, 'scrollWidth', sw);
    else delete (HTMLElement.prototype as { scrollWidth?: number }).scrollWidth;
    if (cw) Object.defineProperty(HTMLElement.prototype, 'clientWidth', cw);
    else delete (HTMLElement.prototype as { clientWidth?: number }).clientWidth;
  }
});
