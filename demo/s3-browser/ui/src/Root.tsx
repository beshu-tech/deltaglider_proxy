import { useEffect, useMemo } from 'react';
import { ConfigProvider } from 'antd';
import App from './App';
import { antdTheme } from './theme';
import { useTheme } from './ThemeContext';

export default function Root() {
  const { isDark, style } = useTheme();
  const theme = useMemo(() => antdTheme(style, isDark), [style, isDark]);
  // Static `message.*` / `Modal.confirm` render outside this tree and
  // cannot read the ConfigProvider context — without a global holder
  // they fall back to AntD's light default palette.
  useEffect(() => {
    ConfigProvider.config({
      holderRender: (children) => <ConfigProvider theme={theme}>{children}</ConfigProvider>,
    });
  }, [theme]);
  return (
    <ConfigProvider theme={theme}>
      <App />
    </ConfigProvider>
  );
}
