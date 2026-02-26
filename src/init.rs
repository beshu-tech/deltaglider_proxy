//! Interactive configuration wizard for `--init` flag.
//!
//! Walks the user through creating a `deltaglider_proxy.toml` file,
//! similar to `npm init` or `cargo init`.

use crate::config::{BackendConfig, Config, ConfigError, TlsConfig};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// Errors that can occur during interactive init.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("Cancelled by user")]
    Cancelled,
}

/// Public entry point wiring stdin/stdout.
pub fn run_interactive_init(default_output_path: &str) -> Result<(), InitError> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut writer = io::stdout();
    run_init_inner(default_output_path, &mut reader, &mut writer)
}

/// Prompt the user for a string value, returning `default` on empty input.
/// Returns `Err(InitError::Cancelled)` on EOF.
fn prompt(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String, InitError> {
    write!(writer, "{} [{}]: ", label, default)?;
    writer.flush()?;
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(InitError::Cancelled);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

/// Prompt for a yes/no answer. Retries on invalid input.
fn prompt_yes_no(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    label: &str,
    default: bool,
) -> Result<bool, InitError> {
    let default_str = if default { "y" } else { "n" };
    loop {
        let answer = prompt(reader, writer, label, default_str)?;
        match answer.to_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {
                writeln!(writer, "  Please answer y or n.")?;
            }
        }
    }
}

/// Prompt for a value that must parse to `T`. Retries on parse failure.
fn prompt_parse<T>(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    label: &str,
    default: T,
) -> Result<T, InitError>
where
    T: std::str::FromStr + std::fmt::Display,
{
    let default_str = default.to_string();
    loop {
        let answer = prompt(reader, writer, label, &default_str)?;
        match answer.parse::<T>() {
            Ok(val) => return Ok(val),
            Err(_) => {
                writeln!(writer, "  Invalid value, please try again.")?;
            }
        }
    }
}

/// Core wizard logic, testable with any `BufRead`/`Write`.
pub fn run_init_inner(
    default_output_path: &str,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<(), InitError> {
    writeln!(writer)?;
    writeln!(writer, "DeltaGlider Proxy - Interactive Configuration")?;
    writeln!(writer, "==============================================")?;
    writeln!(writer)?;

    // --- Output file ---
    let output_path = prompt(reader, writer, "Output config file", default_output_path)?;
    writeln!(writer)?;

    // Check if file already exists
    if std::path::Path::new(&output_path).exists() {
        let overwrite = prompt_yes_no(
            reader,
            writer,
            &format!("{output_path} already exists. Overwrite?"),
            false,
        )?;
        if !overwrite {
            writeln!(writer, "Cancelled.")?;
            return Ok(());
        }
        writeln!(writer)?;
    }

    // --- Server ---
    writeln!(writer, "--- Server ---")?;
    let listen_addr: std::net::SocketAddr = prompt_parse(
        reader,
        writer,
        "Listen address",
        "0.0.0.0:9000".parse().unwrap(),
    )?;
    let log_level = prompt(
        reader,
        writer,
        "Log level",
        "deltaglider_proxy=debug,tower_http=debug",
    )?;

    writeln!(writer)?;

    // --- Backend ---
    writeln!(writer, "--- Backend ---")?;
    let backend = loop {
        let choice = prompt(
            reader,
            writer,
            "Storage backend (filesystem / s3)",
            "filesystem",
        )?;
        match choice.to_lowercase().as_str() {
            "filesystem" | "fs" => {
                let path = prompt(reader, writer, "Data directory", "./data")?;
                break BackendConfig::Filesystem {
                    path: PathBuf::from(path),
                };
            }
            "s3" => {
                let endpoint = prompt(
                    reader,
                    writer,
                    "S3 endpoint URL (empty for AWS default)",
                    "",
                )?;
                let endpoint = if endpoint.is_empty() {
                    None
                } else {
                    Some(endpoint)
                };
                let region = prompt(reader, writer, "AWS region", "us-east-1")?;
                let force_path_style = prompt_yes_no(
                    reader,
                    writer,
                    "Use path-style URLs? (required for MinIO/LocalStack)",
                    true,
                )?;
                let access_key_id = prompt(
                    reader,
                    writer,
                    "Backend AWS access key ID (empty to use env/instance credentials)",
                    "",
                )?;
                let access_key_id = if access_key_id.is_empty() {
                    None
                } else {
                    Some(access_key_id)
                };
                let secret_access_key = if access_key_id.is_some() {
                    let s = prompt(reader, writer, "Backend AWS secret access key", "")?;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                } else {
                    None
                };
                break BackendConfig::S3 {
                    endpoint,
                    region,
                    force_path_style,
                    access_key_id,
                    secret_access_key,
                };
            }
            _ => {
                writeln!(writer, "  Please enter 'filesystem' or 's3'.")?;
            }
        }
    };

    writeln!(writer)?;

    // --- Delta Compression ---
    writeln!(writer, "--- Delta Compression ---")?;
    let max_delta_ratio: f32 = loop {
        let v: f32 = prompt_parse(reader, writer, "Max delta ratio (0.0 - 1.0)", 0.5)?;
        if (0.0..=1.0).contains(&v) {
            break v;
        }
        writeln!(writer, "  Value must be between 0.0 and 1.0.")?;
    };

    let max_object_size_mb: u64 = prompt_parse(reader, writer, "Max object size in MB", 100u64)?;
    let cache_size_mb: usize =
        prompt_parse(reader, writer, "Reference cache size in MB", 100usize)?;

    writeln!(writer)?;

    // --- Proxy Authentication ---
    writeln!(writer, "--- Proxy Authentication ---")?;
    let auth_enabled = prompt_yes_no(reader, writer, "Enable SigV4 authentication?", false)?;
    let (access_key_id, secret_access_key) = if auth_enabled {
        let key = prompt(reader, writer, "Access key ID", "")?;
        let secret = prompt(reader, writer, "Secret access key", "")?;
        (
            if key.is_empty() { None } else { Some(key) },
            if secret.is_empty() {
                None
            } else {
                Some(secret)
            },
        )
    } else {
        (None, None)
    };

    writeln!(writer)?;

    // --- TLS ---
    writeln!(writer, "--- TLS ---")?;
    let tls_enabled = prompt_yes_no(reader, writer, "Enable TLS?", false)?;
    let tls = if tls_enabled {
        let own_cert = prompt_yes_no(reader, writer, "Provide your own certificate?", false)?;
        if own_cert {
            let cert_path = prompt(reader, writer, "Certificate PEM path", "")?;
            let key_path = prompt(reader, writer, "Private key PEM path", "")?;
            Some(TlsConfig {
                enabled: true,
                cert_path: if cert_path.is_empty() {
                    None
                } else {
                    Some(cert_path)
                },
                key_path: if key_path.is_empty() {
                    None
                } else {
                    Some(key_path)
                },
            })
        } else {
            Some(TlsConfig {
                enabled: true,
                cert_path: None,
                key_path: None,
            })
        }
    } else {
        None
    };

    // Build Config
    let config = Config {
        listen_addr,
        backend,
        max_delta_ratio,
        max_object_size: max_object_size_mb * 1024 * 1024,
        cache_size_mb,
        access_key_id,
        secret_access_key,
        admin_password_hash: None,
        codec_concurrency: None,
        blocking_threads: None,
        log_level,
        tls,
    };

    // Show summary
    writeln!(writer)?;
    writeln!(writer, "--- Generated Configuration ---")?;
    let toml_str = config.to_toml_string()?;
    writeln!(writer, "{toml_str}")?;

    // Confirm write
    let do_write = prompt_yes_no(reader, writer, &format!("Write to {output_path}?"), true)?;

    if do_write {
        config.persist_to_file(&output_path)?;
        writeln!(writer, "Configuration written to {output_path}")?;
    } else {
        writeln!(writer, "Cancelled. No file written.")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Helper: run wizard with simulated input, return (output_string, written_file_contents).
    fn run_wizard(input: &str) -> (String, Option<String>) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        // Remove the file so wizard doesn't see it as existing
        std::fs::remove_file(&path).ok();

        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = run_init_inner(&path, &mut reader, &mut output);
        let out_str = String::from_utf8(output).unwrap();

        match result {
            Ok(()) => {
                let contents = std::fs::read_to_string(&path).ok();
                (out_str, contents)
            }
            Err(_) => (out_str, None),
        }
    }

    #[test]
    fn test_defaults_filesystem() {
        // Accept all defaults: output path, listen addr, log level, filesystem,
        // data dir, delta ratio, max obj size, cache size, no auth, no tls, confirm write.
        let input = "\n\n\n\n\n\n\n\nn\nn\ny\n";
        let (output, file) = run_wizard(input);
        assert!(output.contains("DeltaGlider Proxy"));
        let file = file.expect("file should be written");
        assert!(file.contains("listen_addr"));
        assert!(file.contains("filesystem"));
        assert!(file.contains("log_level"));
    }

    #[test]
    fn test_s3_backend() {
        let input = concat!(
            "\n",                      // output path default
            "\n",                      // listen addr default
            "\n",                      // log level default
            "s3\n",                    // backend = s3
            "http://localhost:9000\n", // endpoint
            "eu-west-1\n",             // region
            "y\n",                     // path style
            "\n",                      // no access key
            "\n",                      // delta ratio default
            "\n",                      // max obj size default
            "\n",                      // cache size default
            "n\n",                     // no auth
            "n\n",                     // no tls
            "y\n",                     // confirm write
        );
        let (output, file) = run_wizard(input);
        assert!(output.contains("--- Backend ---"));
        let file = file.expect("file should be written");
        assert!(file.contains("s3"));
        assert!(file.contains("eu-west-1"));
    }

    #[test]
    fn test_cancel_write() {
        // output path, listen addr, log level, backend, data dir, delta ratio,
        // max obj size, cache size, auth(n), tls(n), write(n)
        let input = "\n\n\n\n\n\n\n\nn\nn\nn\n";
        let (output, file) = run_wizard(input);
        assert!(output.contains("Cancelled"));
        assert!(file.is_none());
    }

    #[test]
    fn test_eof_cancels() {
        let input = "";
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        std::fs::remove_file(&path).ok();

        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = run_init_inner(&path, &mut reader, &mut output);
        assert!(matches!(result, Err(InitError::Cancelled)));
    }

    #[test]
    fn test_overwrite_prompt_decline() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();
        std::fs::write(&path, "existing content").unwrap();

        // Accept default output path, then decline overwrite
        let input = "\nn\n";
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        run_init_inner(&path, &mut reader, &mut output).unwrap();

        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Cancelled"));
        // Original file untouched
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "existing content");
    }

    #[test]
    fn test_prompt_helper() {
        let input = "custom_value\n";
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = prompt(&mut reader, &mut output, "Test", "default").unwrap();
        assert_eq!(result, "custom_value");
    }

    #[test]
    fn test_prompt_helper_default() {
        let input = "\n";
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = prompt(&mut reader, &mut output, "Test", "default").unwrap();
        assert_eq!(result, "default");
    }

    #[test]
    fn test_prompt_yes_no_retry() {
        let input = "maybe\ny\n";
        let mut reader = Cursor::new(input.as_bytes().to_vec());
        let mut output = Vec::new();
        let result = prompt_yes_no(&mut reader, &mut output, "Continue?", false).unwrap();
        assert!(result);
        let out_str = String::from_utf8(output).unwrap();
        assert!(out_str.contains("Please answer y or n"));
    }

    #[test]
    fn test_with_auth() {
        let input = concat!(
            "\n",         // output path
            "\n",         // listen addr
            "\n",         // log level
            "\n",         // filesystem
            "\n",         // data dir
            "\n",         // delta ratio
            "\n",         // max obj size
            "\n",         // cache size
            "y\n",        // enable auth
            "mykey\n",    // access key
            "mysecret\n", // secret key
            "n\n",        // no tls
            "y\n",        // confirm write
        );
        let (_, file) = run_wizard(input);
        let file = file.expect("file should be written");
        assert!(file.contains("mykey"));
        assert!(file.contains("mysecret"));
    }
}
