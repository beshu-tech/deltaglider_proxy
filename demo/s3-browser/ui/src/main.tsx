import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import Root from './Root'
import ThemeProvider from './ThemeProvider'
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
      // No focus-refetch burst: every alt-tab back used to re-fire ALL active
      // queries (origins, usage, jobs, mounted panel reads). Live data is
      // covered by explicit pollers; static admin reads refresh on navigation.
      refetchOnWindowFocus: false,
      // A backgrounded tab does NOT keep firing refetchInterval pollers (jobs,
      // parity, maintenance) — they resume on return. Explicit so it can't
      // silently flip; a hidden tab shouldn't hammer the admin API every 2s.
      refetchIntervalInBackground: false,
      retry: 1, // one retry; bigger N hides real backend issues
    },
    mutations: {
      retry: 0, // mutations are not idempotent in general
    },
  },
});

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
        <Root />
      </ThemeProvider>
    </QueryClientProvider>
  </React.StrictMode>,
)
