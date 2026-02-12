import { theme } from 'antd';

export const DG_BRAND = {
  colorPrimary: '#6c5ce7',
  fontFamily: "'Manrope', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
};

export const lightTheme = {
  algorithm: theme.defaultAlgorithm,
  token: { ...DG_BRAND },
};

export const darkTheme = {
  algorithm: theme.darkAlgorithm,
  token: { ...DG_BRAND },
};
