//! CLI subcommands for the `deltaglider_proxy` binary.
//!
//! Top-level shape: `deltaglider_proxy <subcommand> [args...]`.
//!
//! Each subcommand is a small dispatcher that borrows logic from the library
//! crate. Later phases will add `admission trace`, `config apply`,
//! `config show`, etc. — they all follow the same pattern introduced here.

pub mod config;
