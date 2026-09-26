import { defineConfig, mergeConfig } from 'vitest/config'
import viteConfig from './vite.config'

// Two projects: pure-logic tests (`*.test.ts`) run in plain Node — fast, no
// DOM; component and hook tests (`*.test.tsx`) run in jsdom with the AntD shims.
export default mergeConfig(
  viteConfig,
  defineConfig({
    test: {
      restoreMocks: true,
      projects: [
        {
          extends: true,
          test: { name: 'unit', environment: 'node', include: ['src/**/*.test.ts'] },
        },
        {
          extends: true,
          test: {
            name: 'dom',
            environment: 'jsdom',
            include: ['src/**/*.test.tsx'],
            setupFiles: ['src/test/setup.ts'],
            // Full AntD panels take seconds to mount in jsdom; the 5 s
            // default times out on a loaded machine or a 4-vCPU runner.
            testTimeout: 15_000,
          },
        },
      ],
    },
  }),
)
