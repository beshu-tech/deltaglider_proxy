# Command-line tools

The `deltaglider_proxy` binary is both the server and its CLI. With no subcommand it starts the server; with a subcommand (or one of the run-and-exit flags below) it runs to completion and exits without starting the server.

## Flags

| Flag | Effect |
|---|---|
| `-c, --config <FILE>` | Path to the config file (global; also honored by subcommands) |
| `-l, --listen <ADDR>` | Listen address, overrides config |
| `-v, --verbose` | Verbose logging (lowest priority in the log-level resolution order) |
| `--init` | Interactive configuration wizard, then exit |
| `--set-bootstrap-password` | Read a password from stdin, write its bcrypt hash, then exit (alias: `--set-admin-password`) |
| `--show-env` | Print all `DGP_*` environment variables in `.env` format, then exit |
| `--version` | Version plus build timestamp |

## Exit codes

Shared registry across all subcommands: `0` OK, `2` usage, `3` I/O error, `4` parse error, `5` HTTP error, `6` rejected by validation/server, `7` authentication failure, `8` S3 not found, `9` integrity (hash mismatch), `10` partial success in a recursive operation.

## `config lint <FILE>`

Offline validation — the same pipeline as the admin API's `/config/validate`: shape classification, deny-unknown-fields, shorthand normalization, admission-block semantics, `Config::check` warnings. `${env:NAME}` / `${env:NAME:-default}` references are expanded against the environment first; an unset variable without a default fails the lint. YAML is the only supported format; a `.toml` input fails with the TOML-removed error (removed in v1.4.1 — convert with `config migrate` on v1.4.0). Warnings go to stderr and are non-fatal. Exit: `0` valid (with or without warnings), `3` unreadable, `4` parse error, `6` validation error (including an unparseable `log_level` filter).

## `config schema [--out <OUTPUT>]`

Emits the JSON Schema for the canonical `Config` shape (generated from the schemars derives, so it tracks the struct automatically). Consumed by CI and YAML LSP autocompletion.

## `config defaults [--out <OUTPUT>]`

Emits per-field defaults and doc-comment descriptions as JSON Schema. Currently identical output to `config schema`; the two commands are separate entry points and may diverge in future releases.

## `config apply <FILE> [--server <URL>] [--timeout <SECS>]`

Pushes a full YAML document to a running server via `POST /_/api/admin/config/apply`. The server validates, atomically swaps the runtime config, and persists. `${env:NAME}` references are expanded against the *operator's* environment before sending; an empty rendered body is refused.

Authentication is via the `DGP_BOOTSTRAP_PASSWORD` environment variable, not a flag — argv is visible in `ps` listings. The command logs in, holds the session cookie in memory only, and discards it on exit. Defaults: `--server http://127.0.0.1:9000`, `--timeout 30`. A cleartext `http://` URL to a non-loopback host produces a warning. Server-side warnings are echoed to stderr verbatim.

Exit: `0` applied and persisted (with a stderr note when a restart-only field changed); `5` applied in memory but not persisted (also HTTP errors and login rate-limiting); `6` server rejected the apply; `7` missing/wrong `DGP_BOOTSTRAP_PASSWORD`; `3` local I/O error.

## `admission trace --method <M> --path <P> [--authenticated] [--query <Q>] [--server <URL>] [--timeout <SECS>]`

Dry-runs a synthetic request through the running server's admission chain via `POST /_/api/admin/config/trace` and prints the decision as pretty JSON on stdout (pipeable to `jq`). Same `DGP_BOOTSTRAP_PASSWORD` authentication, `--server`/`--timeout` defaults, and exit codes as `config apply`.

## `--init`

Interactive wizard, in the style of `npm init`. Prompts for output path (default `deltaglider_proxy.yaml`), listen address, log level, backend (filesystem or S3 with endpoint/region/credentials), delta settings (`max_delta_ratio`, max object size, cache size), optional SigV4 credentials, and optional TLS. The generated config is printed for confirmation before writing; an existing file requires an explicit overwrite confirmation. The output is always canonical sectioned YAML (a `.toml` output path is refused).

## `--set-bootstrap-password`

Reads one line from stdin, validates password quality, and writes the bcrypt hash to `.deltaglider_bootstrap_hash` in the working directory. Also prints the base64-encoded hash for `DGP_BOOTSTRAP_PASSWORD_HASH` (avoids `$` escaping in Docker/env files). If an encrypted IAM database exists, it becomes unreadable on the next restart — it was encrypted with the old password — and the proxy returns to bootstrap mode. Exit: `0`, or `1` on empty/weak password.

## `s3` — client command family

AWS-CLI-shaped client verbs that talk directly to an S3 endpoint (no running proxy required) and read/write the same delta-storage layout the proxy uses. Metadata written here is bit-compatible with the proxy's.

| Command | Purpose |
|---|---|
| `s3 ls` | List buckets or objects |
| `s3 cp` | Copy between local paths and S3 with transparent delta compression |
| `s3 rm` | Remove objects (single key or recursive prefix delete) |
| `s3 sync` | Sync a directory between local and S3, or between two S3 prefixes |
| `s3 stats` | Bucket statistics: original/stored bytes, savings %, deltaspace health |
| `s3 verify` | SHA256 round-trip integrity check of a stored object |
| `s3 migrate` | Migrate a deltaspace between buckets/accounts through the engine |
| `s3 purge` | Purge expired Python-toolchain rehydration cache entries (`.deltaglider/tmp/*`) |
| `s3 get-bucket-acl` / `s3 put-bucket-acl` | Read / update a bucket ACL (canned-ACL or grant flags) |

Exit codes `8` (not found), `9` (integrity), and `10` (partial) are specific to this family. `--help` on each verb lists its flags.
