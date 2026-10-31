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

// UX harvest C27: the flip painted in two steps (AntD tokens first, the CSS
// variables and the faded row backgrounds later).
test('a toggle turns transitions off until the new theme is painted', () => {
  prefer(false);
  const frames: FrameRequestCallback[] = [];
  vi.spyOn(window, 'requestAnimationFrame').mockImplementation((cb) => frames.push(cb));
  render(<ThemeProvider><Probe /></ThemeProvider>);
  const root = document.documentElement;
  expect(root.classList.contains('dg-theme-switching')).toBe(false);
  act(() => screen.getByRole('button').click());
  expect(root.getAttribute('data-theme')).toBe('dark');
  expect(root.classList.contains('dg-theme-switching')).toBe(true);
  act(() => frames.shift()!(0));
  expect(root.classList.contains('dg-theme-switching')).toBe(true);
  act(() => frames.shift()!(0));
  expect(root.classList.contains('dg-theme-switching')).toBe(false);
});

// A theme is a style times a mode: the style is saved on its own and lands
// on <html data-theme-style>, where the Editorial CSS blocks match.
function StyleProbe() {
  const { style, setStyle, colors } = useTheme();
  return (
    <button onClick={() => setStyle(style === 'editorial' ? 'instrument' : 'editorial')}>
      {style} {colors.BG_BASE}
    </button>
  );
}

test('the style is saved, set on <html>, and picks the palette', () => {
  prefer(false);
  render(<ThemeProvider><StyleProbe /></ThemeProvider>);
  const root = document.documentElement;
  expect(screen.getByRole('button')).toHaveTextContent('instrument #f5f7fa');
  expect(root.getAttribute('data-theme-style')).toBe('instrument');
  act(() => screen.getByRole('button').click());
  expect(screen.getByRole('button')).toHaveTextContent('editorial #fbf6ee');
  expect(root.getAttribute('data-theme-style')).toBe('editorial');
  expect(readStorage('dg-theme-style')).toBe('editorial');
});

test('a saved style is read at start; an unknown one falls back to the default', () => {
  prefer(true);
  window.localStorage.setItem('dg-theme-style', 'editorial');
  const { unmount } = render(<ThemeProvider><StyleProbe /></ThemeProvider>);
  expect(screen.getByRole('button')).toHaveTextContent('editorial #141311');
  unmount();
  window.localStorage.setItem('dg-theme-style', 'neon');
  render(<ThemeProvider><StyleProbe /></ThemeProvider>);
  expect(screen.getByRole('button')).toHaveTextContent('instrument');
});
