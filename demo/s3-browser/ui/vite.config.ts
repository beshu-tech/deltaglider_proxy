import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

// Read the version from the workspace root Cargo.toml so the UI stays
// in lockstep with the Rust crate without a manual bump step. Falls
// back to '?' if the lookup fails — the sidebar surfaces that so it's
// obvious something's wrong with the build.
//
// __BUILD_VERSION__ is also accepted from the environment (DGP_BUILD_VERSION
// or CARGO_PKG_VERSION) so Docker builds can pass it via --build-arg
// without re-reading the file from a context that may not include it.
function resolveBuildVersion(): string {
  const fromEnv =
    process.env.DGP_BUILD_VERSION ||
    process.env.CARGO_PKG_VERSION
  if (fromEnv) return fromEnv
  try {
    const cargoToml = readFileSync(
      resolve(__dirname, '../../../Cargo.toml'),
      'utf8'
    )
    const m = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)
    if (m) return m[1]
  } catch {
    // ignore — we'll fall through to '?'
  }
  return '?'
}

export default defineConfig({
  plugins: [react()],
  base: '/_/',
  define: {
    // ISO 8601 timestamp. Vite evaluates this at config-load time —
    // `npm run build` invokes a fresh Node process so the value IS
    // per-build. Docker builds cache the whole ui-build layer, so a
    // version bump (via the build-arg below) invalidates the cache
    // and forces a fresh build time too.
    __BUILD_TIME__: JSON.stringify(new Date().toISOString()),
    __BUILD_VERSION__: JSON.stringify(resolveBuildVersion()),
  },
  build: {
    sourcemap: true,
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
