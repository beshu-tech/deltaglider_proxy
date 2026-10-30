// Shared render helpers for component tests.
import type { ReactElement, ReactNode } from 'react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { ConfigProvider, type ThemeConfig } from 'antd';
import { render, renderHook, type RenderOptions } from '@testing-library/react';

/** A QueryClient that never retries and never caches between tests. */
export function testQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: 0, staleTime: 0 },
      mutations: { retry: false },
    },
  });
}

/**
 * How long every findBy* / waitFor waits (set in setup.ts). The budget is
 * wall-clock time. A wait whose condition needs later tasks (React work
 * after a dialog closes, a 1 s poll, a chain of mocked requests) gets
 * those tasks late on a loaded host, and the library default of 1 s ran
 * out (CreateBucketModal, firstRun.emptyState, the preview poll). A wait
 * ends as soon as its condition holds, so a longer budget costs only a
 * failing test. Tests do not set a shorter one of their own
 * (asyncWaitBudget.test.ts).
 */
export const ASYNC_WAIT_MS = 5000;

/** AntD theme without motion: jsdom never ends an animation. */
export const NO_MOTION: ThemeConfig = { token: { motion: false } };

function wrapperFor(client: QueryClient) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return (
      <QueryClientProvider client={client}>
        <ConfigProvider theme={NO_MOTION}>{children}</ConfigProvider>
      </QueryClientProvider>
    );
  };
}

export function renderWithQuery(ui: ReactElement, options: RenderOptions & { client?: QueryClient } = {}) {
  const client = options.client ?? testQueryClient();
  return { client, ...render(ui, { wrapper: wrapperFor(client), ...options }) };
}

export function renderHookWithQuery<R>(hook: () => R, client: QueryClient = testQueryClient()) {
  return { client, ...renderHook(hook, { wrapper: wrapperFor(client) }) };
}
