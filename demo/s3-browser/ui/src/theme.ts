import { theme } from 'antd';

export const DG_BRAND = {
  colorPrimary: '#2dd4bf',
  fontFamily: "'Outfit', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
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
    borderRadius: 8,
    fontFamilyCode: "'JetBrains Mono', 'Fira Code', monospace",
  },
};

export const lightTheme = {
  algorithm: theme.defaultAlgorithm,
  token: {
    ...DG_BRAND,
    colorPrimary: '#0d9488',
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
