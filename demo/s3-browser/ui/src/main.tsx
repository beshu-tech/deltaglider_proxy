import React from 'react'
import ReactDOM from 'react-dom/client'
import { ConfigProvider } from 'antd'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import { lightTheme, darkTheme } from './theme'
import { ThemeProvider, useTheme } from './ThemeContext'
import './theme.css'

// Single process-wide query client. Defaults are tuned for an internal
// admin tool: data refetches on focus (operators expect liveness when
// they tab back in) but not in the background, and stale time is short
// so mutations consistently invalidate within the session.
//
// Per-query overrides (longer staleTime, polling intervals) live on
// the query definitions in src/queries/.
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5_000, // 5s: deduplicate concurrent reads from sibling panels
      refetchOnWindowFocus: true,
      retry: 1, // one retry; bigger N hides real backend issues
    },
    mutations: {
      retry: 0, // mutations are not idempotent in general
    },
  },
});

function Root() {
  const { isDark } = useTheme();
  return (
    <ConfigProvider
      theme={isDark ? darkTheme : lightTheme}
    >
      <App />
    </ConfigProvider>
  );
}

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <Root />
      </ThemeProvider>
    </QueryClientProvider>
  </React.StrictMode>,
)
