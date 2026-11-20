import { useEffect, useMemo } from 'react';
import { ConfigProvider } from 'antd';
import App from './App';
import { antdComponentConfig, antdTheme } from './theme';
import { useTheme } from './ThemeContext';

export default function Root() {
  const { isDark, style } = useTheme();
  const theme = useMemo(() => antdTheme(style, isDark), [style, isDark]);
  const components = useMemo(() => antdComponentConfig(style), [style]);
  // Static `message.*` / `Modal.confirm` render outside this tree and
  // cannot read the ConfigProvider context — without a global holder
  // they fall back to AntD's light default palette.
  useEffect(() => {
    ConfigProvider.config({
      holderRender: (children) => <ConfigProvider theme={theme} {...components}>{children}</ConfigProvider>,
    });
  }, [theme, components]);
  return (
    <ConfigProvider theme={theme} {...components}>
      <App />
    </ConfigProvider>
  );
}
