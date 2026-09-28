# Command-line tools

The `deltaglider_proxy` binary is both the server and its CLI. With no subcommand it starts the server; with a subcommand (or one of the run-and-exit flags below) it runs to completion and exits without starting the server.

## Flags

| Flag | Effect |
|---|---|
| `-c, --config <FILE>` | Path to the config file (global; also honored by subcommands) |
| `-l, --listen <ADDR>` | Listen address, overrides config |
| `-v, --verbose` | Verbose logging (`trace`). `RUST_LOG`, `DGP_LOG_LEVEL` and a `log_level` that the config file sets win over it |
| `--init` | Interactive configuration wizard, then exit |
| `--set-bootstrap-password` | Read a password from stdin, write its bcrypt hash, then exit (alias: `--set-admin-password`) |
| `--show-env` | Print all `DGP_*` environment variables in `.env` format, then exit |
| `--version` | Version plus build timestamp |

## Exit codes

All subcommands share one set of exit codes: `0` OK, `2` usage, `3` I/O error, `4` parse error, `5` HTTP error, `6` rejected by validation/server, `7` authentication failure, `8` S3 not found, `9` integrity (hash mismatch), `10` partial success in a recursive operation.

## `config lint`

```text
deltaglider_proxy config lint <FILE>
```

`config lint` validates a file offline. It uses the same pipeline as the admin API's `/config/validate`: shape classification, deny-unknown-fields, shorthand normalization, admission-block semantics, `Config::check` warnings (including the cross-field [config advisories](configuration.md#config-advisories): shared rate-limit bucket, stale IAM template, frozen quota, redundant public prefix). The command first expands `${env:NAME}` / `${env:NAME:-default}` references against the environment. An unset variable without a default fails the lint. YAML is the only supported format. A `.toml` input fails with the TOML-removed error (TOML support was removed in v1.4.1; convert the file with `config migrate` on v1.4.0). Warnings go to stderr and are non-fatal. An unknown root key of a flat-shape document is an error, although the proxy loads such a file and ignores the key. Exit: `0` valid (with or without warnings), `3` unreadable, `4` parse error (also an unknown key, an unset `${env:NAME}` without a default, and an empty or whitespace-only file, which an apply would refuse because it resets every field), `6` validation error (including an unparseable `log_level` filter, and a lifecycle or replication rule that an apply refuses, such as two replication rules with the same name).

## `config schema`

```text
deltaglider_proxy config schema [--out <OUTPUT>]
```

Emits the JSON Schema for the canonical sectioned document, with the `admission`, `access`, `storage` and `advanced` sections at the root. This is the shape that `config export` and every admin save write. The schema comes from the schemars derives, so it follows the code automatically. Every field that is not text, such as a boolean, a number or a choice from a list, also accepts a whole `${env:NAME}` or `${env:NAME:-default}` reference, because the proxy gives the expanded value the type of the field. CI and YAML LSP autocompletion consume the schema.

## `config defaults`

```text
deltaglider_proxy config defaults [--out <OUTPUT>]
```

Emits per-field defaults and doc-comment descriptions as JSON Schema. The output is currently identical to that of `config schema`. The two commands are separate entry points, and their output may diverge in future releases.

## `config apply`

```text
deltaglider_proxy config apply <FILE> [--server <URL>] [--timeout <SECS>]
```

Sends a full YAML document to a running server with `POST /_/api/admin/config/apply`. The server validates the document, swaps the runtime config atomically, and persists it. Before the command sends the document, it expands `${env:NAME}` references against the environment of the operator. The command refuses an empty rendered body.

The command reads the password from the `DGP_BOOTSTRAP_PASSWORD` environment variable. It does not take the password as a flag, because `ps` listings show the command line. The command logs in, holds the session cookie in memory only, and discards it on exit. Defaults: `--server http://127.0.0.1:9000`, `--timeout 30`. A cleartext `http://` URL to a non-loopback host produces a warning. The command echoes the warnings of the server to stderr verbatim.

Exit: `0` applied and persisted (with a stderr note when a restart-only field changed); `5` applied in memory but not persisted (also HTTP errors and login rate-limiting); `6` server rejected the apply; `7` missing/wrong `DGP_BOOTSTRAP_PASSWORD`; `3` local I/O error.

## `admission trace`

```text
deltaglider_proxy admission trace --method <M> --path <P> [--authenticated] [--query <Q>] [--server <URL>] [--timeout <SECS>]
```

Sends a synthetic request through the admission chain of the running server as a dry run, with `POST /_/api/admin/config/trace`. The command prints the decision as pretty JSON on stdout, which you can pipe to `jq`. It uses the same `DGP_BOOTSTRAP_PASSWORD` authentication, `--server`/`--timeout` defaults, and exit codes as `config apply`.

## `--init`

`--init` starts an interactive wizard in the style of `npm init`. The wizard prompts for the output path (default `deltaglider_proxy.yaml`), listen address, log level, backend (filesystem or S3 with endpoint/region/credentials), delta settings (`max_delta_ratio`, max object size, cache size), optional SigV4 credentials, and optional TLS. Every file that the wizard writes starts the proxy. When you enable SigV4 authentication and leave the key prompts empty, the wizard generates the key pair and prints the secret. The proxy refuses to start without credentials unless the file sets `authentication: none`. For this reason, when you decline authentication, the wizard writes `authentication: none`. Use that file for development only. The wizard prints the generated config for confirmation before it writes the file. It asks for an explicit confirmation before it overwrites an existing file. The output is always canonical sectioned YAML (a `.toml` output path is refused).

## `--set-bootstrap-password`

Reads one line from stdin, validates password quality, and writes the bcrypt hash to `.deltaglider_bootstrap_hash` in the working directory. It also prints the base64-encoded hash for `DGP_BOOTSTRAP_PASSWORD_HASH` (avoids `$` escaping in Docker/env files). The flag does not change the IAM database, because the key of the database is `DGP_CONFIG_DB_KEY` or the key file next to the database. If the database is still encrypted with the old hash (a database from a release before the config DB key), the flag first re-encrypts it with the config DB key. Exit: `0`, or `1` on empty/weak password, or when the database opens with neither the config DB key nor the current hash (the hash is then not changed).

## `s3` client command family

The `s3` verbs are client commands shaped like the AWS CLI. They talk directly to an S3 endpoint (no running proxy is required), and they read and write the same delta-storage layout that the proxy uses. The metadata that these verbs write is bit-compatible with the metadata that the proxy writes.

**Point these verbs at the storage backend, not at the proxy.** Each verb runs its own copy of the delta engine, so it writes and reads the delta-storage layout (`reference.bin` baselines and `.delta` objects) itself. The proxy refuses requests for these internal key names from S3 clients, and it already applies delta compression to every object that it stores. For these two reasons, the `s3` verbs cannot work through the proxy. Before a verb (`ls`, `cp`, `rm`, `sync`, `stats`, `verify`, `migrate`, `purge`, `get-bucket-acl`, `put-bucket-acl`) sends a request, it sends one unauthenticated `GET /_/health` to `--endpoint-url`. When the answer comes from a DeltaGlider Proxy, the verb stops with exit code `2` and a message that says so. To store objects through the proxy, use any plain S3 client, for example `aws s3 --endpoint-url https://s3.acme.example`.

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

### Credentials

The `s3` verbs read credentials in this order: the `--access-key-id` and `--secret-access-key` flags, then the `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and `AWS_SESSION_TOKEN` environment variables, then the profile in `~/.aws/credentials` (selected by `--profile`, then `AWS_PROFILE`, then `default`). A session token from the environment or from the profile's `aws_session_token` is sent with every request, so temporary (STS) credentials work.

### Recursive prefixes

`cp -r`, `rm -r`, `sync`, and `migrate` treat a non-empty source prefix as a directory. `s3://releases/v2` and `s3://releases/v2/` both select only the keys under `v2/`. They do not select keys under a sibling such as `v2-rc/`, and they do not select an object whose key is exactly `v2`. This rule is stricter than `aws s3 rm --recursive`, which matches the raw prefix.

`rm -r` deletes each key with its own request, folder markers (keys that end with `/`) included. It also deletes the folder marker of the directory itself, such as the key `v2/`, when no `--include` or `--exclude` glob narrows the delete.

A download (`cp -r` or `sync` from S3 to a local directory) writes only below the destination directory. The CLI skips a key that contains an empty path segment (for example a leading `/` or `//`), a `.` or `..` segment, a backslash, a drive letter such as `C:`, or a NUL byte. It prints a warning for each skipped key, and the command exits with `10` (partial) or `5`. The CLI also skips folder markers (keys that end with `/`) without a warning.

### Include and exclude globs

`--include` and `--exclude` match the key relative to the source prefix, in `cp -r`, `rm -r`, `sync`, and `migrate`. A glob without a `/` matches the last path segment of the key (`*.zip`). A glob with a `/` matches the whole relative key: `rm -r s3://releases/v2/ --exclude 'tmp/*'` keeps `v2/tmp/`. When a key matches both an include and an exclude glob, the exclude glob wins.

### S3-to-S3 copies

`cp`, `sync`, and `migrate` between two S3 locations keep the source object's Content-Type and user metadata. A `--metadata K=V` flag on `cp` replaces the value of that key. Cache-Control, Content-Disposition, and Content-Encoding are not copied.

### Object size and memory

`cp`, `sync`, `migrate`, and `verify` do not hold a large object in memory. An object of 8 MiB or less is read into memory, because a spool copy costs more than it saves for a small file. A larger upload is first copied into a spool file (in `DGP_SPOOL_DIR`, within the `DGP_SPOOL_MAX_BYTES` budget) and then stored from that file. A download writes the object to a temporary file beside the destination and renames the file into place at the end, so a failed download does not leave a truncated file. An S3-to-S3 copy writes the source into a spool file first. `verify` hashes the object while it reads it. The engine itself reconstructs a delta object in memory when the object is 16 MiB or smaller, and in a spool file when it is larger. The verbs do not read `DGP_SPOOL_THRESHOLD_BYTES` or the proxy config file, so this threshold is fixed; it is lower only when `--max-object-size-mb` is lower. The verbs do read `DGP_SPOOL_DIR` and `DGP_SPOOL_MAX_BYTES`.

`--no-delta` on `cp`, `sync`, and `migrate` stores each object as a plain object, without a delta against the `reference.bin` baseline. The verb sends the user metadata `dg-no-delta: true`, and the engine does not store this hint with the object. An S3 client that stores through the proxy can send the same hint as the header `x-amz-meta-dg-no-delta: true`.

`--max-object-size-mb` sets the size limit for delta-eligible objects only (default 100 MiB), because the xdelta3 memory use grows with the object size. Other files are limited to 64 GiB, the default of `max_passthrough_object_size`. The verbs do not read the proxy config file, so no setting changes this limit.

### `s3 verify` results

| Output | Exit code | Meaning |
|---|---|---|
| `OK` | `0` | The SHA-256 of the downloaded bytes matches the checksum that DeltaGlider stored with the object. |
| `UNVERIFIABLE` | `0` | The object has no DeltaGlider checksum, because another tool wrote it. The download succeeded, and the line shows the SHA-256 of the bytes. |
| `MISMATCH` | `9` | The SHA-256 of the downloaded bytes differs from the stored checksum, or the engine found a checksum error while it reconstructed a delta. |
| `error: object not found` or `error: bucket not found` | `8` | The key or the bucket does not exist. |
