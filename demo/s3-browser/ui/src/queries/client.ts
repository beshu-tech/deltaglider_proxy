import { QueryClient } from '@tanstack/react-query';

// Single process-wide query client. Defaults are tuned for an internal
// admin tool: data refetches on focus (operators expect liveness when
// they tab back in) but not in the background, and stale time is short
// so mutations consistently invalidate within the session.
//
// Per-query overrides (longer staleTime, polling intervals) live on
// the query definitions in src/queries/. Code outside React (s3client's
// listBuckets) reads shared queries through it too.
export const queryClient = new QueryClient({
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
