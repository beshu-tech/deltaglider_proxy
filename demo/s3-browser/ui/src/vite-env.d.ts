/// <reference types="vite/client" />
// vite/client supplies the ambient module declarations for side-effect CSS
// imports (`import './theme.css'`). Deliberately nothing else here: build
// constants (a version, a build time) must not exist in this bundle — see
// vite.config.ts and scripts/check-bundle-fingerprints.sh.
