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
