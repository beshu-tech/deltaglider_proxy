import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { darkColors, lightColors, ThemeContext } from './ThemeContext';
import { readStorage, writeStorage } from './safeStorage';

const DARK_QUERY = '(prefers-color-scheme: dark)';
/** On `<html>` for the two frames of a theme flip (theme.css turns transitions off). */
const THEME_SWITCHING_CLASS = 'dg-theme-switching';

function systemPrefersDark(): boolean {
  return typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia(DARK_QUERY).matches
    : true;
}

export default function ThemeProvider({ children }: { children: ReactNode }) {
  // A saved choice wins; without one, follow the system setting (and keep
  // following it: nothing is saved until the user picks a theme).
  const [saved, setSaved] = useState(() => readStorage('dg-theme'));
  const [systemDark, setSystemDark] = useState(systemPrefersDark);
  const isDark = saved ? saved === 'dark' : systemDark;

  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return;
    const mq = window.matchMedia(DARK_QUERY);
    const onChange = (e: MediaQueryListEvent) => setSystemDark(e.matches);
    mq.addEventListener?.('change', onChange);
    return () => mq.removeEventListener?.('change', onChange);
  }, []);

  // Stable identity so the context value (and therefore every `useColors()` /
  // `useTheme()` consumer — i.e. nearly the whole tree) only churns on an actual
  // theme change, not on every ThemeProvider render.
  const toggleTheme = useCallback(() => {
    setSaved((prev) => {
      const next = (prev ? prev === 'dark' : systemPrefersDark()) ? 'light' : 'dark';
      writeStorage('dg-theme', next);
      return next;
    });
  }, []);
  const colors = isDark ? darkColors : lightColors;

  // One paint for the whole flip. AntD injects its new tokens during render;
  // a plain effect set `data-theme` (our CSS variables) after the paint, and
  // the per-element background transitions faded the rest in later, so a
  // frame showed a dark table header over light rows. A layout effect runs
  // before the paint, and `dg-theme-switching` turns transitions off until
  // the new colours are on screen.
  const firstRun = useRef(true);
  useLayoutEffect(() => {
    const root = document.documentElement;
    root.setAttribute('data-theme', isDark ? 'dark' : 'light');
    if (firstRun.current) {
      firstRun.current = false;
      return;
    }
    root.classList.add(THEME_SWITCHING_CLASS);
    let frame = requestAnimationFrame(() => {
      frame = requestAnimationFrame(() => root.classList.remove(THEME_SWITCHING_CLASS));
    });
    return () => {
      cancelAnimationFrame(frame);
      root.classList.remove(THEME_SWITCHING_CLASS);
    };
  }, [isDark]);

  const value = useMemo(() => ({ isDark, toggleTheme, colors }), [isDark, toggleTheme, colors]);

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}
