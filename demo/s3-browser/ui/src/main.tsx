import React from 'react'
import ReactDOM from 'react-dom/client'
import { ConfigProvider } from 'antd'
import App from './App'
import { lightTheme, darkTheme } from './theme'
import { ThemeProvider, useTheme } from './ThemeContext'
import './theme.css'

// Dedicated portal container for Ant Design popups (Select, AutoComplete, etc.).
// Must sit outside any CSS-transformed ancestors — transforms create new
// containing blocks that break position:absolute/fixed calculations in
// rc-component/trigger, causing popups to render off-screen.
const popupRoot = document.createElement('div');
popupRoot.id = 'antd-popup-root';
document.body.appendChild(popupRoot);

function Root() {
  const { isDark } = useTheme();
  return (
    <ConfigProvider
      theme={isDark ? darkTheme : lightTheme}
      getPopupContainer={() => popupRoot}
    >
      <App />
    </ConfigProvider>
  );
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <ThemeProvider>
      <Root />
    </ThemeProvider>
  </React.StrictMode>,
)
