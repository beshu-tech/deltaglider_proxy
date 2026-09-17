import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Production builds ship no source maps: a map in the embedded bundle hands
// the full UI source to anonymous callers. The Dockerfile sets PROD=true for
// its UI build stage (`--build-arg PROD=false` restores the maps for a debug
// image); a plain local `npm run build` keeps them.
function isProdBuild(): boolean {
  return ['1', 'true', 'yes', 'on'].includes((process.env.PROD ?? '').trim().toLowerCase())
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
    // ON for dev builds (debugging the embedded admin UI), OFF under PROD —
    // DemoAssets embeds all of dist/, so a map in dist/ is a map on prod.
    sourcemap: !isProdBuild(),
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
