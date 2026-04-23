/** Injected by Vite at build time — ISO 8601 timestamp of the build. */
declare const __BUILD_TIME__: string;

/**
 * Injected by Vite at build time from the workspace root `Cargo.toml`
 * (with `DGP_BUILD_VERSION` / `CARGO_PKG_VERSION` env vars as overrides
 * for CI). The UI uses this to stay in lockstep with the Rust crate
 * version without a manual bump step. Will be `"?"` if the lookup
 * failed, which the sidebar surfaces as-is.
 */
declare const __BUILD_VERSION__: string;
