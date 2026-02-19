import { createContext, useContext, useState, useEffect, type ReactNode } from 'react';

// Dark colors — deep navy instrument panel
const darkColors = {
  BG_BASE: '#080c14',
  BG_SIDEBAR: '#0b1120',
  BG_CARD: '#111827',
  BG_ELEVATED: '#162032',
  BORDER: '#1e2d45',
  TEXT_PRIMARY: '#e4e9f2',
  TEXT_SECONDARY: '#8b99b0',
  TEXT_MUTED: '#5c6d85',
  TEXT_FAINT: '#3d4f66',
  ACCENT_BLUE: '#2dd4bf',     // teal as primary accent
  ACCENT_BLUE_LIGHT: '#5eead4',
  ACCENT_GREEN: '#34d399',
  ACCENT_RED: '#fb7185',
  ACCENT_PURPLE: '#a78bfa',
  ACCENT_AMBER: '#fbbf24',
  STORAGE_TYPE_COLORS: {
    delta:     { bg: 'rgba(167, 139, 250, 0.1)', border: 'rgba(167, 139, 250, 0.25)', text: '#a78bfa' },
    reference: { bg: 'rgba(56, 189, 248, 0.1)',  border: 'rgba(56, 189, 248, 0.25)',  text: '#38bdf8' },
    passthrough: { bg: 'rgba(52, 211, 153, 0.1)',  border: 'rgba(52, 211, 153, 0.25)',  text: '#34d399' },
  } as Record<string, { bg: string; border: string; text: string }>,
  STORAGE_TYPE_DEFAULT: { bg: 'rgba(52, 211, 153, 0.1)', border: 'rgba(52, 211, 153, 0.25)', text: '#34d399' },
};

// Light colors — clean, high-contrast
const lightColors = {
  BG_BASE: '#f5f7fa',
  BG_SIDEBAR: '#edf0f5',
  BG_CARD: '#ffffff',
  BG_ELEVATED: '#ffffff',
  BORDER: '#d5dbe5',
  TEXT_PRIMARY: '#0c1629',
  TEXT_SECONDARY: '#475569',
  TEXT_MUTED: '#64748b',
  TEXT_FAINT: '#94a3b8',
  ACCENT_BLUE: '#0d9488',     // darker teal for contrast on light
  ACCENT_BLUE_LIGHT: '#0f766e',
  ACCENT_GREEN: '#059669',
  ACCENT_RED: '#e11d48',
  ACCENT_PURPLE: '#7c3aed',
  ACCENT_AMBER: '#d97706',
  STORAGE_TYPE_COLORS: {
    delta:     { bg: '#f3e8ff', border: '#c084fc', text: '#7c3aed' },
    reference: { bg: '#e0f2fe', border: '#38bdf8', text: '#0284c7' },
    passthrough: { bg: '#d1fae5', border: '#34d399', text: '#059669' },
  } as Record<string, { bg: string; border: string; text: string }>,
  STORAGE_TYPE_DEFAULT: { bg: '#d1fae5', border: '#34d399', text: '#059669' },
};

export type ColorTokens = typeof darkColors;

interface ThemeContextValue {
  isDark: boolean;
  toggleTheme: () => void;
  colors: ColorTokens;
}

const ThemeContext = createContext<ThemeContextValue>({
  isDark: true,
  toggleTheme: () => {},
  colors: darkColors,
});

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [isDark, setIsDark] = useState(() => {
    const saved = localStorage.getItem('dg-theme');
    return saved ? saved === 'dark' : true;
  });

  const toggleTheme = () => setIsDark(prev => !prev);
  const colors = isDark ? darkColors : lightColors;

  useEffect(() => {
    localStorage.setItem('dg-theme', isDark ? 'dark' : 'light');
    document.documentElement.setAttribute('data-theme', isDark ? 'dark' : 'light');
  }, [isDark]);

  return (
    <ThemeContext.Provider value={{ isDark, toggleTheme, colors }}>
      {children}
    </ThemeContext.Provider>
  );
}

export function useColors() {
  const { colors } = useContext(ThemeContext);
  return colors;
}

export function useTheme() {
  return useContext(ThemeContext);
}
