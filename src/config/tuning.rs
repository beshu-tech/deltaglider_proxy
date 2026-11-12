// SPDX-License-Identifier: BUSL-1.1

//! Environment-only settings that the request path reads.
//!
//! These `DGP_*` variables have no YAML field. They are parsed once per
//! config snapshot, in `Config::apply_env_overrides_tracked` (at boot
//! and on every admin apply), and stored in [`super::Config::tuning`]. The
//! engine copies the snapshot at build, so a request reads a field and never
//! the process environment. Same names, same defaults as before.

use super::env_overrides::parse_bool;
use super::{lookup_bool, lookup_parse, EnvLookup};
use crate::transfer_plan as tp;

/// The xdelta3 subprocess deadlines (see `deltaglider::codec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecTimeouts {
    /// `DGP_CODEC_TIMEOUT_SECS`: wall clock of a buffered encode/decode.
    pub buffered_secs: u64,
    /// `DGP_CODEC_STALL_SECS`: no-progress limit of a streaming op.
    pub stall_secs: u64,
    /// `DGP_CODEC_ABSOLUTE_SECS`: hard ceiling of a streaming op.
    pub absolute_secs: u64,
}

impl Default for CodecTimeouts {
    fn default() -> Self {
        Self {
            buffered_secs: 60,
            stall_secs: 30,
            absolute_secs: 2 * 60 * 60,
        }
    }
}

/// The `/_/ready` probe policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyProbe {
    /// `DGP_READY_TIMEOUT_SECS` (at least 1).
    pub timeout_secs: u64,
    /// `DGP_READY_RETRIES`.
    pub retries: u32,
    /// `DGP_READY_CACHE_TTL_SECS` (0 = strict; never negative).
    pub cache_ttl_secs: i64,
}

/// The default of `DGP_SHUTDOWN_TIMEOUT_SECS`. The drain steps before this
/// wait take up to 10 s each, and the detached-work wait up to 15 s, so the
/// longest stop is 10 + 15 + 10 + 10 + 10 = 55 s.
/// The Helm chart gives a pod 60 s (`terminationGracePeriodSeconds`), which is
/// more than that sum.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 10;

/// The default of `DGP_STATE_LOCK_WAIT_SECS`: how long a new process waits
/// for the old one to release the state directory lock.
pub const DEFAULT_STATE_LOCK_WAIT_SECS: u64 = 60;

/// The default of `DGP_BACKEND_SHARE_PERCENT`.
pub const DEFAULT_BACKEND_SHARE_PERCENT: u8 = 75;

/// Env-only settings of one config snapshot. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTuning {
    /// `DGP_REQUEST_TIMEOUT_SECS`: the S3 router timeout, also the drain
    /// bound of a maintenance job's in-flight writes.
    pub request_timeout_secs: u64,
    /// `DGP_MAX_CONCURRENT_REQUESTS`.
    pub max_concurrent_requests: usize,
    /// `DGP_HEADER_READ_TIMEOUT_SECS` (at least 1): the time a client has
    /// to send the head of a request, and the keep-alive idle limit.
    pub header_read_timeout_secs: u64,
    /// `DGP_MAX_CONNECTIONS` (at least 1): the most connections open at
    /// once. `None`: 4 x `max_concurrent_requests`, at least 4096
    /// (`serve::ListenerLimits`).
    pub max_connections: Option<usize>,
    /// `DGP_BACKEND_SHARE_PERCENT` (1-100): the most of the request slots
    /// that the requests to one backend may hold when several backends are
    /// configured. The spool reads the same variable for its budget.
    pub backend_share_percent: u8,
    /// `DGP_CORS_PERMISSIVE` (dev mode).
    pub cors_permissive: bool,
    /// `DGP_DEBUG_HEADERS`.
    pub debug_headers: bool,
    /// `DGP_TRUST_PROXY_HEADERS`.
    pub trust_proxy_headers: bool,
    /// `DGP_IAM_SERVE_UNCONFIRMED`: with a coordination bucket, serve IAM
    /// users from the local database before one sync confirmed it. Off by
    /// default: a peer may have removed a user since the last run.
    pub iam_serve_unconfirmed: bool,
    /// `DGP_OUTBOUND_USE_SYSTEM_PROXY`: the webhook, Slack and OIDC clients
    /// send their requests through the system HTTP(S) proxy
    /// (`HTTP(S)_PROXY`, `ALL_PROXY`). Off by default: with a proxy the
    /// connect-time address check does not see the target.
    pub outbound_via_system_proxy: bool,
    /// `DGP_SECURE_COOKIES`: a recognised value wins; `None` (unset or not
    /// a boolean) falls through to TLS / forwarded-proto detection.
    pub secure_cookies: Option<bool>,
    /// `DGP_CLOCK_SKEW_SECONDS`: the SigV4 skew s3s enforces.
    pub clock_skew_secs: u32,
    /// `DGP_REPLAY_WINDOW_SECS`: how long a mutating request's signature is
    /// refused after its first copy. Default: twice the skew, because s3s
    /// accepts a signature dated up to the skew ahead, so one first used at
    /// t0 verifies until t0 + 2 x skew. 0 turns the check off.
    pub replay_window_secs: u64,
    pub codec: CodecTimeouts,
    /// `DGP_SPOOL_THRESHOLD_BYTES`; `None` = the engine's default for its
    /// `max_object_size`.
    pub spool_threshold_bytes: Option<u64>,
    /// `DGP_SPOOL_ACQUIRE_TIMEOUT_SECS`.
    pub spool_acquire_timeout_secs: u64,
    /// `DGP_SPOOL_MAX_BYTES`: the spool budget. The spool and the config
    /// check read the same variable.
    pub spool_max_bytes: u64,
    /// `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`: what one multipart upload may hold
    /// in the spool (`Some(0)` = no cap; `None` = half of the spool).
    pub multipart_relay_upload_max: Option<u64>,
    /// `DGP_MAX_TOTAL_MULTIPART_BYTES`: what all open multipart uploads may
    /// hold together (`None` = half of the spool).
    pub multipart_total_max: Option<u64>,
    /// `DGP_SHUTDOWN_TIMEOUT_SECS` (at least 1): after the drain steps, the
    /// time that the exit waits for tasks on the blocking pool (an xdelta3
    /// run can take 2 hours). Read before the runtime exists.
    pub shutdown_timeout_secs: u64,
    /// `DGP_STATE_LOCK_WAIT_SECS` (0 = no wait): how long the boot waits
    /// for another process to release the state directory lock.
    pub state_lock_wait_secs: u64,
    /// `DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES`.
    pub mpu_delta_reconstruct_max_bytes: u64,
    /// `DGP_STREAM_COPY_THRESHOLD`: a passthrough copy at least this
    /// large streams via multipart (at least 1).
    pub stream_copy_threshold: u64,
    /// `DGP_MULTIPART_PART_SIZE`: part size of that copy (at least the
    /// S3 minimum).
    pub multipart_part_size: u64,
    /// `DGP_UPLOAD_CONCURRENCY`: in-flight parts per streaming copy that
    /// sets no value of its own (at least 1).
    pub upload_concurrency: usize,
    pub ready: ReadyProbe,
    /// `DGP_REFERENCE_SCAN_LIMIT`: the most references one savings chip
    /// counts. It finds them on its listing pages, with no HEAD.
    pub reference_scan_limit: usize,
    /// The variable that pins the bootstrap hash ([`env_pinned_hash_var`]).
    pub bootstrap_hash_env: Option<&'static str>,
    /// `DGP_ACCESS_KEY_ID` or `DGP_SECRET_ACCESS_KEY` is set: the bootstrap
    /// SigV4 pair comes from the environment.
    pub bootstrap_pair_from_env: bool,
}

impl Default for RuntimeTuning {
    /// The defaults: an empty environment.
    fn default() -> Self {
        Self::from_env(&|_| None)
    }
}

impl RuntimeTuning {
    /// Parse every field from `env`. Pure apart from the warnings the
    /// parse helpers print for a value that does not parse.
    pub fn from_env(env: EnvLookup) -> Self {
        let parse_or = |var: &str, default: u64| lookup_parse(env, var).unwrap_or(default);
        let clock_skew_secs: u32 = lookup_parse(env, "DGP_CLOCK_SKEW_SECONDS")
            .unwrap_or(900)
            .max(1);
        let codec = CodecTimeouts::default();
        Self {
            // 0 would time every request out at once.
            request_timeout_secs: parse_or("DGP_REQUEST_TIMEOUT_SECS", 300).max(1),
            // 0 would admit no request: the proxy would hang every one.
            max_concurrent_requests: lookup_parse(env, "DGP_MAX_CONCURRENT_REQUESTS")
                .unwrap_or(1024)
                .max(1),
            header_read_timeout_secs: parse_or(
                "DGP_HEADER_READ_TIMEOUT_SECS",
                crate::serve::DEFAULT_HEADER_READ_TIMEOUT.as_secs(),
            )
            .max(1),
            max_connections: lookup_parse::<usize>(env, "DGP_MAX_CONNECTIONS").map(|n| n.max(1)),
            backend_share_percent: lookup_parse::<u8>(env, "DGP_BACKEND_SHARE_PERCENT")
                .unwrap_or(DEFAULT_BACKEND_SHARE_PERCENT)
                .clamp(1, 100),
            cors_permissive: lookup_bool(env, "DGP_CORS_PERMISSIVE", false),
            debug_headers: lookup_bool(env, "DGP_DEBUG_HEADERS", false),
            trust_proxy_headers: lookup_bool(env, "DGP_TRUST_PROXY_HEADERS", false),
            iam_serve_unconfirmed: lookup_bool(env, "DGP_IAM_SERVE_UNCONFIRMED", false),
            outbound_via_system_proxy: lookup_bool(env, "DGP_OUTBOUND_USE_SYSTEM_PROXY", false),
            secure_cookies: env("DGP_SECURE_COOKIES").and_then(|raw| parse_bool(&raw)),
            clock_skew_secs,
            // Twice the skew: s3s accepts a signature dated up to the skew
            // ahead, so one first used at t0 verifies until t0 + 2 x skew.
            replay_window_secs: parse_or("DGP_REPLAY_WINDOW_SECS", 2 * u64::from(clock_skew_secs)),
            codec: CodecTimeouts {
                // 0 would kill every xdelta3 run at its first check.
                buffered_secs: parse_or("DGP_CODEC_TIMEOUT_SECS", codec.buffered_secs).max(1),
                stall_secs: parse_or("DGP_CODEC_STALL_SECS", codec.stall_secs).max(1),
                absolute_secs: parse_or("DGP_CODEC_ABSOLUTE_SECS", codec.absolute_secs).max(1),
            },
            spool_threshold_bytes: lookup_parse(env, "DGP_SPOOL_THRESHOLD_BYTES"),
            spool_acquire_timeout_secs: parse_or("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", 120).max(1),
            spool_max_bytes: parse_or(
                "DGP_SPOOL_MAX_BYTES",
                crate::deltaglider::spool::DEFAULT_SPOOL_MAX_BYTES,
            ),
            // The names are `multipart::RELAY_UPLOAD_MAX_ENV` and
            // `MAX_TOTAL_MULTIPART_ENV` (a test checks it).
            multipart_relay_upload_max: lookup_parse(env, "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES"),
            multipart_total_max: lookup_parse(env, "DGP_MAX_TOTAL_MULTIPART_BYTES"),
            // 0 would abandon the blocking tasks at once.
            shutdown_timeout_secs: parse_or(
                "DGP_SHUTDOWN_TIMEOUT_SECS",
                DEFAULT_SHUTDOWN_TIMEOUT_SECS,
            )
            .max(1),
            state_lock_wait_secs: parse_or(
                "DGP_STATE_LOCK_WAIT_SECS",
                DEFAULT_STATE_LOCK_WAIT_SECS,
            ),
            mpu_delta_reconstruct_max_bytes: parse_or(
                "DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES",
                64 * 1024 * 1024,
            ),
            // Floored at 1: a zero threshold would admit a 0-byte object, and
            // plan_parts(0) is empty → a zero-part CompleteMultipartUpload.
            stream_copy_threshold: parse_or("DGP_STREAM_COPY_THRESHOLD", tp::STREAM_COPY_THRESHOLD)
                .max(1),
            multipart_part_size: parse_or("DGP_MULTIPART_PART_SIZE", tp::MULTIPART_PART_SIZE)
                .max(tp::S3_MIN_PART_SIZE),
            upload_concurrency: lookup_parse(env, "DGP_UPLOAD_CONCURRENCY")
                .unwrap_or(tp::UPLOAD_CONCURRENCY)
                .max(1),
            ready: ReadyProbe {
                timeout_secs: parse_or("DGP_READY_TIMEOUT_SECS", 3).max(1),
                retries: lookup_parse(env, "DGP_READY_RETRIES").unwrap_or(2),
                cache_ttl_secs: lookup_parse::<i64>(env, "DGP_READY_CACHE_TTL_SECS")
                    .unwrap_or(0)
                    .max(0),
            },
            reference_scan_limit: lookup_parse(env, "DGP_REFERENCE_SCAN_LIMIT")
                .unwrap_or(crate::deltaglider::REFERENCE_SCAN_LIMIT),
            bootstrap_hash_env: env_pinned_hash_var(env),
            bootstrap_pair_from_env: ["DGP_ACCESS_KEY_ID", "DGP_SECRET_ACCESS_KEY"]
                .iter()
                .any(|v| env(v).is_some()),
        }
    }

    /// `DGP_REPLAY_WINDOW_SECS` as a duration.
    pub fn replay_window(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.replay_window_secs)
    }
}

/// D15: the env var that pins the bootstrap hash, if one is set. That hash
/// wins at every boot, so a GUI change would be lost at the next start.
pub fn env_pinned_hash_var(env: impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    ["DGP_BOOTSTRAP_PASSWORD_HASH", "DGP_ADMIN_PASSWORD_HASH"]
        .into_iter()
        .find(|n| env(n).is_some_and(|v| !v.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn from(pairs: &[(&str, &str)]) -> RuntimeTuning {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        RuntimeTuning::from_env(&move |n: &str| map.get(n).cloned())
    }

    /// What `0` means for each numeric `DGP_*` setting that the request path
    /// reads: a floor of 1 (a zero would stop the proxy), or `Zero` where 0
    /// is a documented value. A new setting must be added here (the source
    /// test below fails when a variable is missing).
    enum Zero {
        /// 0 is read as 1.
        Floor1,
        /// 0 is a documented value (off, or no limit).
        Meaningful(&'static str),
    }

    const ZERO_RULES: &[(&str, Zero)] = &[
        ("DGP_REQUEST_TIMEOUT_SECS", Zero::Floor1),
        ("DGP_MAX_CONCURRENT_REQUESTS", Zero::Floor1),
        ("DGP_HEADER_READ_TIMEOUT_SECS", Zero::Floor1),
        ("DGP_MAX_CONNECTIONS", Zero::Floor1),
        ("DGP_BACKEND_SHARE_PERCENT", Zero::Floor1),
        // 0 would refuse every signed request (RequestTimeTooSkewed).
        ("DGP_CLOCK_SKEW_SECONDS", Zero::Floor1),
        (
            "DGP_REPLAY_WINDOW_SECS",
            Zero::Meaningful("replay check off"),
        ),
        ("DGP_CODEC_TIMEOUT_SECS", Zero::Floor1),
        ("DGP_CODEC_STALL_SECS", Zero::Floor1),
        ("DGP_CODEC_ABSOLUTE_SECS", Zero::Floor1),
        (
            "DGP_SPOOL_THRESHOLD_BYTES",
            Zero::Meaningful("every body takes the spool path"),
        ),
        ("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", Zero::Floor1),
        (
            "DGP_SPOOL_MAX_BYTES",
            Zero::Meaningful("the spool admits no body"),
        ),
        (
            "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES",
            Zero::Meaningful("no cap for one multipart upload"),
        ),
        (
            "DGP_MAX_TOTAL_MULTIPART_BYTES",
            Zero::Meaningful("no multipart part is admitted"),
        ),
        ("DGP_SHUTDOWN_TIMEOUT_SECS", Zero::Floor1),
        (
            "DGP_STATE_LOCK_WAIT_SECS",
            Zero::Meaningful("the boot does not wait for the state directory lock"),
        ),
        (
            "DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES",
            Zero::Meaningful("no delta is rebuilt"),
        ),
        ("DGP_STREAM_COPY_THRESHOLD", Zero::Floor1),
        ("DGP_MULTIPART_PART_SIZE", Zero::Floor1),
        ("DGP_UPLOAD_CONCURRENCY", Zero::Floor1),
        ("DGP_READY_TIMEOUT_SECS", Zero::Floor1),
        ("DGP_READY_RETRIES", Zero::Meaningful("no retry")),
        (
            "DGP_READY_CACHE_TTL_SECS",
            Zero::Meaningful("strict, no cache"),
        ),
        (
            "DGP_REFERENCE_SCAN_LIMIT",
            Zero::Meaningful("the chip counts no reference"),
        ),
    ];

    /// The system proxy for the webhook, Slack and OIDC clients is an
    /// opt-in: off in an empty environment.
    #[test]
    fn the_outbound_system_proxy_is_opt_in() {
        assert!(!from(&[]).outbound_via_system_proxy);
        assert!(from(&[("DGP_OUTBOUND_USE_SYSTEM_PROXY", "true")]).outbound_via_system_proxy);
        assert!(!from(&[("DGP_OUTBOUND_USE_SYSTEM_PROXY", "no")]).outbound_via_system_proxy);
    }

    /// `0` reads as `1` on every `Floor1` setting: the whole tuning struct
    /// with the variable at `0` equals the one with it at `1` (zero is the
    /// value that turned `DGP_MAX_CONCURRENT_REQUESTS=0` into a hang).
    #[test]
    fn a_zero_never_stops_the_proxy() {
        let mut floors = 0;
        for (var, rule) in ZERO_RULES {
            match rule {
                Zero::Floor1 => {
                    floors += 1;
                    assert_eq!(
                        from(&[(var, "0")]),
                        from(&[(var, "1")]),
                        "{var}=0 must read as 1"
                    );
                }
                Zero::Meaningful(why) => {
                    assert!(!why.is_empty(), "{var} documents what 0 means");
                }
            }
        }
        assert!(floors >= 10, "the table holds the Floor1 rows");
    }

    /// Every numeric `DGP_*` variable that this file reads has a row in
    /// [`ZERO_RULES`]: a new setting cannot skip the question "what does 0 do?".
    #[test]
    fn every_numeric_variable_has_a_zero_rule() {
        let src = include_str!("tuning.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap()];
        // Drop comment lines, then take the first quoted `DGP_` name after
        // each numeric read, wherever the call breaks its lines.
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut vars = std::collections::BTreeSet::new();
        for marker in ["parse_or(", "lookup_parse"] {
            for (at, _) in code.match_indices(marker) {
                let tail = &code[at + marker.len()..];
                let window = crate::security::str_prefix(tail, 160);
                if let Some(q) = window.find("\"DGP_") {
                    let name: String = window[q + 1..].chars().take_while(|c| *c != '"').collect();
                    vars.insert(name);
                }
            }
        }
        assert!(
            vars.len() >= 20,
            "scan found the numeric variables: {vars:?}"
        );
        let ruled: std::collections::BTreeSet<String> =
            ZERO_RULES.iter().map(|(v, _)| v.to_string()).collect();
        let missing: Vec<_> = vars.difference(&ruled).collect();
        assert!(missing.is_empty(), "add a ZERO_RULES row for: {missing:?}");
        let stale: Vec<_> = ruled.difference(&vars).collect();
        assert!(stale.is_empty(), "ZERO_RULES rows nothing reads: {stale:?}");
    }

    /// B109: s3s accepts a signature dated up to the skew AHEAD, so one
    /// first used at t0 still verifies at t0 + 2 x skew. The default window
    /// must refuse its replay that long.
    #[test]
    fn the_default_replay_window_covers_a_future_dated_signature() {
        let t = RuntimeTuning::default();
        assert!(
            t.replay_window_secs >= 2 * u64::from(t.clock_skew_secs),
            "window {} < 2 x skew {}",
            t.replay_window_secs,
            t.clock_skew_secs
        );
    }

    #[test]
    fn empty_env_gives_the_documented_defaults() {
        let t = RuntimeTuning::default();
        assert_eq!(t.request_timeout_secs, 300);
        assert_eq!(t.max_concurrent_requests, 1024);
        assert!(!t.cors_permissive && !t.debug_headers && !t.trust_proxy_headers);
        assert_eq!(t.secure_cookies, None);
        assert_eq!((t.clock_skew_secs, t.replay_window_secs), (900, 1800));
        assert_eq!(t.codec, CodecTimeouts::default());
        assert_eq!(
            (
                t.codec.buffered_secs,
                t.codec.stall_secs,
                t.codec.absolute_secs
            ),
            (60, 30, 7200)
        );
        assert_eq!(t.spool_threshold_bytes, None);
        assert_eq!(t.spool_acquire_timeout_secs, 120);
        assert_eq!(t.shutdown_timeout_secs, DEFAULT_SHUTDOWN_TIMEOUT_SECS);
        assert_eq!(t.state_lock_wait_secs, DEFAULT_STATE_LOCK_WAIT_SECS);
        assert_eq!(t.mpu_delta_reconstruct_max_bytes, 64 * 1024 * 1024);
        assert_eq!(
            (
                t.stream_copy_threshold,
                t.multipart_part_size,
                t.upload_concurrency
            ),
            (
                crate::transfer_plan::STREAM_COPY_THRESHOLD,
                crate::transfer_plan::MULTIPART_PART_SIZE,
                crate::transfer_plan::UPLOAD_CONCURRENCY
            )
        );
        assert_eq!(
            t.ready,
            ReadyProbe {
                timeout_secs: 3,
                retries: 2,
                cache_ttl_secs: 0
            }
        );
        assert_eq!(
            t.reference_scan_limit,
            crate::deltaglider::REFERENCE_SCAN_LIMIT
        );
        assert_eq!(t.bootstrap_hash_env, None);
        assert!(!t.bootstrap_pair_from_env);
    }

    #[test]
    fn every_variable_is_read() {
        let t = from(&[
            ("DGP_REQUEST_TIMEOUT_SECS", "7"),
            ("DGP_MAX_CONCURRENT_REQUESTS", "8"),
            ("DGP_CORS_PERMISSIVE", "yes"),
            ("DGP_DEBUG_HEADERS", "1"),
            ("DGP_TRUST_PROXY_HEADERS", "on"),
            ("DGP_SECURE_COOKIES", "off"),
            ("DGP_CLOCK_SKEW_SECONDS", "60"),
            ("DGP_CODEC_TIMEOUT_SECS", "9"),
            ("DGP_CODEC_STALL_SECS", "10"),
            ("DGP_CODEC_ABSOLUTE_SECS", "11"),
            ("DGP_SPOOL_THRESHOLD_BYTES", "12"),
            ("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", "13"),
            ("DGP_SHUTDOWN_TIMEOUT_SECS", "17"),
            ("DGP_STATE_LOCK_WAIT_SECS", "19"),
            ("DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES", "14"),
            ("DGP_READY_TIMEOUT_SECS", "0"),
            ("DGP_READY_RETRIES", "5"),
            ("DGP_READY_CACHE_TTL_SECS", "-4"),
            ("DGP_REFERENCE_SCAN_LIMIT", "3"),
            ("DGP_STREAM_COPY_THRESHOLD", "15"),
            ("DGP_MULTIPART_PART_SIZE", "6291456"),
            ("DGP_UPLOAD_CONCURRENCY", "16"),
        ]);
        assert_eq!((t.request_timeout_secs, t.max_concurrent_requests), (7, 8));
        assert!(t.cors_permissive && t.debug_headers && t.trust_proxy_headers);
        assert_eq!(t.secure_cookies, Some(false));
        // The replay window follows the skew unless set on its own.
        assert_eq!((t.clock_skew_secs, t.replay_window_secs), (60, 120));
        assert_eq!(
            t.codec,
            CodecTimeouts {
                buffered_secs: 9,
                stall_secs: 10,
                absolute_secs: 11
            }
        );
        assert_eq!(t.spool_threshold_bytes, Some(12));
        assert_eq!(t.spool_acquire_timeout_secs, 13);
        assert_eq!(t.shutdown_timeout_secs, 17);
        assert_eq!(t.state_lock_wait_secs, 19);
        assert_eq!(t.mpu_delta_reconstruct_max_bytes, 14);
        // Clamped: at least one second, never a negative TTL.
        assert_eq!(
            t.ready,
            ReadyProbe {
                timeout_secs: 1,
                retries: 5,
                cache_ttl_secs: 0
            }
        );
        assert_eq!(t.reference_scan_limit, 3);
        assert_eq!(
            (
                t.stream_copy_threshold,
                t.multipart_part_size,
                t.upload_concurrency
            ),
            (15, 6 * 1024 * 1024, 16)
        );
        assert!(from(&[("DGP_SECRET_ACCESS_KEY", "x")]).bootstrap_pair_from_env);
        assert_eq!(
            from(&[("DGP_BOOTSTRAP_PASSWORD_HASH", "$2b$x")]).bootstrap_hash_env,
            Some("DGP_BOOTSTRAP_PASSWORD_HASH")
        );
        assert_eq!(
            from(&[("DGP_REPLAY_WINDOW_SECS", "0")]).replay_window_secs,
            0
        );
    }

    /// The snapshot follows the environment at load AND at every admin
    /// apply: an edit parsed from YAML carries no tuning of its own.
    #[test]
    fn the_config_snapshot_follows_the_env_through_load_and_apply() {
        let env = |n: &str| (n == "DGP_DEBUG_HEADERS").then(|| "true".to_string());
        let mut running = crate::config::Config::default();
        running.apply_env_overrides_at_load(&env);
        assert!(running.tuning.debug_headers);

        let mut edited: crate::config::Config =
            serde_yaml::from_str("max_delta_ratio: 0.5\n").unwrap();
        assert!(!edited.tuning.debug_headers, "YAML never carries it");
        edited.reapply_env_overrides(&running, &env).unwrap();
        assert!(edited.tuning.debug_headers);
    }

    /// A zero threshold would stream an empty object as a zero-part
    /// upload, a part below the S3 minimum is illegal, and zero in-flight
    /// parts never finishes a copy.
    #[test]
    fn streaming_copy_settings_are_floored() {
        let t = from(&[
            ("DGP_STREAM_COPY_THRESHOLD", "0"),
            ("DGP_MULTIPART_PART_SIZE", "1024"),
            ("DGP_UPLOAD_CONCURRENCY", "0"),
        ]);
        assert_eq!(t.stream_copy_threshold, 1);
        assert_eq!(
            t.multipart_part_size,
            crate::transfer_plan::S3_MIN_PART_SIZE
        );
        assert_eq!(t.upload_concurrency, 1);
    }

    #[test]
    fn secure_cookies_is_tri_state() {
        for (raw, want) in [
            ("true", Some(true)),
            (" ON ", Some(true)),
            ("0", Some(false)),
            ("maybe", None),
            ("", None),
        ] {
            assert_eq!(
                from(&[("DGP_SECURE_COOKIES", raw)]).secure_cookies,
                want,
                "{raw:?}"
            );
        }
    }
}
