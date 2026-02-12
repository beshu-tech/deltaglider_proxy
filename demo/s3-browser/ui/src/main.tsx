import React, { useState } from 'react'
import ReactDOM from 'react-dom/client'
import { ConfigProvider } from 'antd'
import App from './App'
import { lightTheme, darkTheme } from './theme'
import './theme.css'

function Root() {
  const [isDark, setIsDark] = useState(true);
  return (
    <ConfigProvider theme={isDark ? darkTheme : lightTheme}>
      <App isDark={isDark} onToggleTheme={() => setIsDark(!isDark)} />
    </ConfigProvider>
  );
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
)
