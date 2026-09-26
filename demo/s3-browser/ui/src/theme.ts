import { theme } from 'antd';

const DG_BRAND = {
  colorPrimary: '#2dd4bf',
  fontFamily: "'Manrope', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
};

export const darkTheme = {
  algorithm: theme.darkAlgorithm,
  token: {
    ...DG_BRAND,
    colorBgBase: '#080c14',
    colorBgContainer: '#111827',
    colorBgElevated: '#162032',
    colorBorderSecondary: '#1e2d45',
    colorSuccess: '#34d399',
    colorError: '#fb7185',
    colorWarning: '#fbbf24',
    colorTextBase: '#e4e9f2',
    // WCAG AA (4.5:1): text on the teal primary is dark, not white (2.5:1),
    // and the derived secondary/placeholder greys are lifted.
    colorTextLightSolid: '#0b1120',
    colorTextSecondary: '#8b93a1',
    colorTextTertiary: '#8b93a1',
    colorTextPlaceholder: '#858f9e',
    fontSize: 14,
    borderRadius: 8,
    fontFamilyCode: "'JetBrains Mono', 'Fira Code', monospace",
  },
};

export const lightTheme = {
  algorithm: theme.defaultAlgorithm,
  token: {
    ...DG_BRAND,
    // #0d9488 under white button text is 3.7:1; #0f766e is 5.5:1 (WCAG AA).
    colorPrimary: '#0f766e',
    colorTextSecondary: '#55627a',
    colorTextTertiary: '#55627a',
    colorTextPlaceholder: '#5f6c80',
    colorBgBase: '#f5f7fa',
    colorBgContainer: '#ffffff',
    colorBgElevated: '#ffffff',
    colorBorderSecondary: '#d5dbe5',
    colorSuccess: '#059669',
    colorError: '#e11d48',
    colorWarning: '#d97706',
    colorTextBase: '#0c1629',
    borderRadius: 8,
    fontFamilyCode: "'JetBrains Mono', 'Fira Code', monospace",
  },
};
