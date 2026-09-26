import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { darkColors, lightColors, ThemeContext } from './ThemeContext';
import { readStorage, writeStorage } from './safeStorage';

const DARK_QUERY = '(prefers-color-scheme: dark)';

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

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', isDark ? 'dark' : 'light');
  }, [isDark]);

  const value = useMemo(() => ({ isDark, toggleTheme, colors }), [isDark, toggleTheme, colors]);

  return <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>;
}
