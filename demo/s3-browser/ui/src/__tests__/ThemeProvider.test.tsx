/**
 * Browser-review item 24: with no saved choice the UI was always dark. It
 * now follows prefers-color-scheme, and it saves a theme only when the user
 * picks one (so a later system change still applies until then).
 */
import { act, render, screen } from '@testing-library/react';
import { afterEach, expect, test, vi } from 'vitest';
import ThemeProvider from '../ThemeProvider';
import { useTheme } from '../ThemeContext';
import { readStorage } from '../safeStorage';

function Probe() {
  const { isDark, toggleTheme } = useTheme();
  return <button onClick={toggleTheme}>{isDark ? 'dark' : 'light'}</button>;
}

function prefer(dark: boolean) {
  vi.spyOn(window, 'matchMedia').mockImplementation((q: string) => ({
    matches: q === '(prefers-color-scheme: dark)' ? dark : false,
    media: q, onchange: null, addListener: vi.fn(), removeListener: vi.fn(),
    addEventListener: vi.fn(), removeEventListener: vi.fn(), dispatchEvent: vi.fn(),
  }) as unknown as MediaQueryList);
}

afterEach(() => {
  window.localStorage.clear();
  vi.restoreAllMocks();
});

test('no saved theme + a light system → light, nothing saved', () => {
  prefer(false);
  render(<ThemeProvider><Probe /></ThemeProvider>);
  expect(screen.getByRole('button')).toHaveTextContent('light');
  expect(document.documentElement.getAttribute('data-theme')).toBe('light');
  expect(readStorage('dg-theme')).toBeNull();
});

test('no saved theme + a dark system → dark; a toggle is saved', () => {
  prefer(true);
  render(<ThemeProvider><Probe /></ThemeProvider>);
  expect(screen.getByRole('button')).toHaveTextContent('dark');
  act(() => screen.getByRole('button').click());
  expect(screen.getByRole('button')).toHaveTextContent('light');
  expect(readStorage('dg-theme')).toBe('light');
});
