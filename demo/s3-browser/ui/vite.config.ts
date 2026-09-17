import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Source maps are OPT-IN. DemoAssets embeds all of dist/ into the binary and
// serves it to anonymous callers, so a map in dist/ is the full UI source on
// every deployment — the Docker image, the release tarballs and the demo
// image alike. No build channel has to remember a flag: set
// DGP_UI_SOURCEMAP=1 for a local debugging build.
function sourcemapOptIn(): boolean {
  return ['1', 'true', 'yes', 'on'].includes((process.env.DGP_UI_SOURCEMAP ?? '').trim().toLowerCase())
}

// Source maps are OPT-IN. DemoAssets embeds all of dist/ into the binary and
// serves it to anonymous callers, so a map in dist/ is the full UI source on
// every deployment — the Docker image, the release tarballs and the demo
// image alike. No build channel has to remember a flag: set
// DGP_UI_SOURCEMAP=1 for a local debugging build.
function sourcemapOptIn(): boolean {
  return ['1', 'true', 'yes', 'on'].includes((process.env.DGP_UI_SOURCEMAP ?? '').trim().toLowerCase())
}

export default defineConfig({
  plugins: [react()],
  base: '/_/',
  // Deliberately NO `define` of a build version or build time: every
  // literal baked into the bundle is served to anonymous callers and would
  // fingerprint the deployment. The running version and build time come from
  // the session-authenticated `/_/api/whoami` at runtime instead, and
  // scripts/check-bundle-fingerprints.sh fails the build if either creeps
  // back into dist/.
  build: {
    sourcemap: sourcemapOptIn(),
    //
    // manualChunks: split heavy vendor libs out of the main shell so
    // the file-browser entry only downloads what it needs on first
    // paint. AntD, AWS SDK, markdown stack, and dnd-kit are all
    // independently cacheable across page navigations.
    //
    // Function form (not the object form) because Vite 8 ships Rolldown
    // as its bundler, and Rolldown's manualChunks only accepts a
    // function. The function form is portable — it also works under
    // the classic Rollup backend, so no Vite-version coupling here.
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes('node_modules')) return
          // Chunk name -> packages whose node_modules path matches.
          const groups: [string, string[]][] = [
            ['antd', ['antd', '@ant-design/icons']],
            ['aws-sdk', [
              '@aws-sdk/client-s3',
              '@aws-sdk/lib-storage',
              '@aws-sdk/s3-request-presigner',
            ]],
            ['markdown', [
              'react-markdown',
              'remark-gfm',
              'rehype-highlight',
              'rehype-slug',
            ]],
            ['dnd', ['@dnd-kit/core', '@dnd-kit/sortable', '@dnd-kit/utilities']],
            ['forms', ['react-hook-form', '@hookform/resolvers', 'zod']],
          ]
          for (const [chunk, pkgs] of groups) {
            if (pkgs.some((p) => id.includes(`/node_modules/${p}/`))) return chunk
          }
        },
      },
    },
  },
  server: {
    proxy: {
      '/_/api': 'http://localhost:9000',
      '/_/health': 'http://localhost:9000',
      '/_/stats': 'http://localhost:9000',
      '/_/metrics': 'http://localhost:9000',
    },
  },
})
