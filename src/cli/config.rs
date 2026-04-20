//! `deltaglider_proxy config <subcommand>` dispatcher.
//!
//! Phase 0 ships `config migrate`. Phase 1/2/4 will extend this module with
//! `apply`, `show`, `defaults`, `lint`, `explain`, and `trace`.

use crate::config::{Config, ConfigError, ConfigFormat};
use std::io::Write;
use std::path::Path;

/// Exit codes for CLI subcommands. Stable contract for CI scripts.
pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_IO: i32 = 3;
pub const EXIT_PARSE: i32 = 4;

/// `config migrate <input> [--out <output>]`
///
/// Loads a TOML (or YAML) config file and emits the canonical YAML form.
/// When `--out` is omitted, YAML is written to stdout. Secrets are stripped
/// before serialization (same policy as the admin API export path).
pub fn migrate(input: &str, output: Option<&str>) -> i32 {
    if !Path::new(input).exists() {
        eprintln!("error: input file not found: {input}");
        return EXIT_IO;
    }

    let config = match Config::from_file(input) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to parse {input}: {e}");
            return EXIT_PARSE;
        }
    };

    let yaml = match config.to_canonical_yaml() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to serialize to YAML: {e}");
            return EXIT_PARSE;
        }
    };

    match output {
        Some(path) => match std::fs::write(path, &yaml) {
            Ok(()) => {
                eprintln!("migrated {input} -> {path}");
                if ConfigFormat::from_path(path) != ConfigFormat::Yaml {
                    eprintln!(
                        "note: output extension is not .yaml/.yml; the file \
                         contains YAML content regardless."
                    );
                }
                EXIT_OK
            }
            Err(e) => {
                eprintln!("error: failed to write {path}: {e}");
                EXIT_IO
            }
        },
        None => {
            // Write directly to stdout so callers can pipe.
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(e) = lock.write_all(yaml.as_bytes()) {
                eprintln!("error: failed to write to stdout: {e}");
                return EXIT_IO;
            }
            EXIT_OK
        }
    }
}

/// Adapter for callers that prefer `Result<(), ConfigError>` over exit codes.
pub fn migrate_to_string(input: &str) -> Result<String, ConfigError> {
    Config::from_file(input)?.to_canonical_yaml()
}

/// `config schema [--out <path>]`
///
/// Emit the JSON Schema for the canonical Config shape. Produced from
/// `schemars` derives at build time, so the schema tracks the struct
/// automatically. Used by CI to publish `schema/deltaglider.schema.json` and
/// by YAML LSP / editor autocomplete.
pub fn schema(output: Option<&str>) -> i32 {
    let schema = schemars::schema_for!(Config);
    let pretty = match serde_json::to_string_pretty(&schema) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to serialize schema: {e}");
            return EXIT_PARSE;
        }
    };
    match output {
        Some(path) => match std::fs::write(path, &pretty) {
            Ok(()) => {
                eprintln!("wrote schema to {path}");
                EXIT_OK
            }
            Err(e) => {
                eprintln!("error: failed to write {path}: {e}");
                EXIT_IO
            }
        },
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(e) = lock.write_all(pretty.as_bytes()) {
                eprintln!("error: failed to write to stdout: {e}");
                return EXIT_IO;
            }
            EXIT_OK
        }
    }
}

/// Produce the JSON Schema as a String (used by tests and callers that want
/// the schema without spawning the process).
pub fn schema_string() -> Result<String, ConfigError> {
    let schema = schemars::schema_for!(Config);
    serde_json::to_string_pretty(&schema).map_err(|e| ConfigError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn migrate_toml_to_yaml_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("cfg.toml");
        std::fs::write(
            &toml_path,
            r#"
listen_addr = "0.0.0.0:9001"
max_delta_ratio = 0.4

[backend]
type = "filesystem"
path = "/tmp/dgp"
"#,
        )
        .unwrap();

        let yaml = migrate_to_string(toml_path.to_str().unwrap()).unwrap();
        assert!(yaml.contains("9001"));
        assert!(yaml.contains("0.4"));

        let reparsed: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(reparsed.listen_addr.port(), 9001);
        assert!((reparsed.max_delta_ratio - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn migrate_yaml_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("cfg.toml");
        std::fs::write(&toml_path, "listen_addr = \"127.0.0.1:9000\"\n").unwrap();

        let yaml_a = migrate_to_string(toml_path.to_str().unwrap()).unwrap();

        let yaml_path = dir.path().join("cfg.yaml");
        let mut f = std::fs::File::create(&yaml_path).unwrap();
        f.write_all(yaml_a.as_bytes()).unwrap();

        let yaml_b = migrate_to_string(yaml_path.to_str().unwrap()).unwrap();
        assert_eq!(yaml_a, yaml_b, "YAML → YAML migrate must be idempotent");
    }

    #[test]
    fn migrate_strips_infra_secrets() {
        // `migrate` must match `to_toml_string` / `to_canonical_yaml`:
        // strip bootstrap hash and encryption key (infra secrets), keep SigV4
        // creds so the output is drop-in-usable by the wizard path.
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("cfg.toml");
        std::fs::write(
            &toml_path,
            r#"
access_key_id = "AKIAKEEPME"
secret_access_key = "runtime-key-kept-for-file-use"
bootstrap_password_hash = "$2b$12$xxxxxxxxxxxxxxxxxxxxxx"
encryption_key = "deadbeef-hex-key"
"#,
        )
        .unwrap();

        let yaml = migrate_to_string(toml_path.to_str().unwrap()).unwrap();
        // Infra secrets stripped
        assert!(!yaml.contains("$2b$"));
        assert!(!yaml.contains("deadbeef-hex-key"));
        // SigV4 runtime creds survive — migration output must remain a
        // drop-in YAML equivalent of the input TOML.
        assert!(yaml.contains("AKIAKEEPME"));
        assert!(yaml.contains("runtime-key-kept-for-file-use"));
    }
}
