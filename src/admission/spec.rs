//! Operator-authored admission block wire format.
//!
//! This module is the **YAML-facing** layer for the admission chain.
//! It defines the serde shapes an operator writes in their config file
//! (or POSTs to `/api/admin/config/apply`). The runtime enums
//! ([`crate::admission::Match`] / [`crate::admission::Action`]) and
//! the evaluator ([`crate::admission::evaluator`]) live separately —
//! [`crate::admission::AdmissionChain::from_config_parts`] compiles
//! [`AdmissionBlockSpec`] values into the runtime form on every chain
//! build.
//!
//! # Why separate from the runtime enums?
//!
//! The runtime enums are narrow by design: each new predicate or action
//! requires updating the evaluator, the middleware, and the trace
//! endpoint in lock-step. Keeping the serde shape in its own module
//! lets the wire format grow optional fields without forcing a
//! coordinated runtime change.
//!
//! # Wire format
//!
//! ```yaml
//! admission:
//!   blocks:
//!     - name: "deny-known-bad-ips"
//!       match:
//!         source_ip_list: ["203.0.113.5", "198.51.100.0/24"]
//!       action: deny
//!
//!     - name: "maintenance-mode"
//!       match:
//!         config_flag: "maintenance_mode"
//!       action:
//!         type: reject
//!         status: 503
//!         message: "we'll be right back"
//!
//!     - name: "allow-releases"
//!       match:
//!         method: [GET, HEAD]
//!         bucket: "releases"
//!         path_glob: "*.zip"
//!       action: allow-anonymous
//! ```
//!
//! # Invariants
//!
//! * **`deny_unknown_fields`** on every struct so typos surface loudly.
//! * IP/CIDR strings are validated at parse time (typo in a denylist
//!   should fail on load, not silently match nothing forever).
//! * Status codes in `Reject` must be 4xx or 5xx. `200-OK-reject`
//!   blocks are almost certainly operator error.
//! * Block names are unique across the chain — order still matters,
//!   but identical names in trace output would be useless.

use ipnet::IpNet;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

/// Wire format for an operator-authored admission block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdmissionBlockSpec {
    /// Human-readable identifier used by trace output and logs. Must be
    /// unique across the chain — two blocks with the same name are
    /// indistinguishable in diagnostics.
    pub name: String,

    /// Predicate that fires the block. An empty `match: {}` fires on
    /// every request, which is rarely what an operator wants — usually
    /// meant for the chain's terminal fallback; other positions should
    /// narrow the predicate.
    ///
    /// Renamed to `match:` on the wire — `match_` is a Rust-only tweak
    /// to dodge the keyword collision.
    #[serde(default, rename = "match")]
    pub match_: MatchSpec,

    /// What the block does when its predicate matches.
    pub action: ActionSpec,
}

/// Predicates an operator can combine on a single block. All fields are
/// AND'd together — a block with `method: [GET]` AND `bucket: "releases"`
/// fires only for GETs on the `releases` bucket. To express OR, use
/// separate blocks with the same action.
///
/// Field names here mirror the plan doc exactly. Stable names let the
/// operator's YAML survive future additions; new predicates become new
/// optional fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MatchSpec {
    /// HTTP methods the request must use. `None` = any method. Case-
    /// insensitive comparison at evaluation time; we uppercase on parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<Vec<String>>,

    /// Exact source IP. Mutually exclusive with the list / CIDR forms
    /// — setting more than one is a parse-time error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<IpAddr>,

    /// Source IP list. Accepts both bare IPs (`203.0.113.5`) and CIDRs
    /// (`198.51.100.0/24`). Bare IPs are promoted to `/32` or `/128`
    /// during parse, so the evaluator only deals with a uniform
    /// `Vec<IpNet>` at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip_list: Option<Vec<SourceIpEntry>>,

    /// Bucket the request targets. Lowercased on parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,

    /// Glob-style pattern matched against the **full object key** (i.e.
    /// everything after the bucket name). `"*.zip"`, `"releases/**"`,
    /// `"docs/readme.md"`. Not set = match any key (including bucket-
    /// level LIST requests, where the key is empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,

    /// Fire only when the request is / is not authenticated. `None` =
    /// either. Useful for "apply rate limit only to anonymous
    /// traffic".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated: Option<bool>,

    /// Fire only while a named config flag is true. The flag registry
    /// is not yet live (the `maintenance_mode` flag is recognised by
    /// the compile step but always evaluates false at runtime — a
    /// warning fires at chain-build time so operators see the gap).
    /// Unrecognised flag names fire a separate per-block warning.
    /// Full dispatch lands with the Phase 3b.2.c rate-limit work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_flag: Option<String>,
}

/// Source-IP entry accepting bare IPs and CIDRs. Parsed via a serde
/// dance (string → either `IpAddr` or `IpNet`) so the operator's YAML
/// reads naturally: `- 203.0.113.5` and `- 198.51.100.0/24` both work.
///
/// The original string form is preserved so round-tripping through
/// export → re-apply doesn't silently rewrite `203.0.113.5/32` to
/// `203.0.113.5`. GitOps diffs flipping back and forth on every apply
/// are a pattern that burns operator trust; we keep the operator's
/// authored form verbatim and only compute `IpNet` for evaluator use.
#[derive(Debug, Clone)]
pub struct SourceIpEntry {
    /// Parsed network (bare IPs promoted to /32 or /128 for uniform
    /// evaluator dispatch). Public so the evaluator can match
    /// against it directly without re-parsing.
    pub net: IpNet,
    /// Operator's authored string, re-emitted verbatim on serialize.
    /// Preserves `/32` vs bare, case in IPv6, etc.
    raw: String,
}

impl SourceIpEntry {
    /// Construct from an `IpNet` with the canonical display form as
    /// the `raw` source. Intended for programmatic callers (tests,
    /// future GUI persistence); YAML deserialize preserves whatever
    /// the operator wrote.
    pub fn from_net(net: IpNet) -> Self {
        Self {
            net,
            raw: net.to_string(),
        }
    }
}

impl PartialEq for SourceIpEntry {
    fn eq(&self, other: &Self) -> bool {
        // Semantic equality on the network; raw-string differences
        // don't matter for "same block".
        self.net == other.net
    }
}

impl Serialize for SourceIpEntry {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for SourceIpEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        // Try CIDR first; fall back to bare IP (promoted to /32 or /128).
        if let Ok(net) = raw.parse::<IpNet>() {
            return Ok(SourceIpEntry { net, raw });
        }
        if let Ok(addr) = raw.parse::<IpAddr>() {
            let net = match addr {
                IpAddr::V4(v4) => IpNet::V4(ipnet::Ipv4Net::new(v4, 32).expect("32 is valid")),
                IpAddr::V6(v6) => IpNet::V6(ipnet::Ipv6Net::new(v6, 128).expect("128 is valid")),
            };
            return Ok(SourceIpEntry { net, raw });
        }
        Err(serde::de::Error::custom(format!(
            "invalid IP or CIDR: `{}` (expected e.g. `203.0.113.5` or `198.51.100.0/24`)",
            raw
        )))
    }
}

impl schemars::JsonSchema for SourceIpEntry {
    fn schema_name() -> String {
        "SourceIpEntry".to_string()
    }
    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        // Reads as a string in the schema — the parse-time validation
        // is a serde concern, not a schema concern.
        <String as schemars::JsonSchema>::json_schema(gen)
    }
}

/// Action variants operators author. Mirrors the runtime
/// [`crate::admission::Action`] but with operator-provided configuration
/// (status code, message). The chain builder translates this into
/// the runtime form via
/// [`crate::admission::AdmissionChain::from_config_parts`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", untagged)]
pub enum ActionSpec {
    /// Short form: `action: allow-anonymous` / `action: deny` /
    /// `action: continue`. Kebab-case for YAML ergonomics.
    Simple(SimpleAction),

    /// Long form for actions that carry configuration, tagged by
    /// `type:`. Example:
    ///
    /// ```yaml
    /// action:
    ///   type: reject
    ///   status: 503
    ///   message: "we'll be right back"
    /// ```
    Tagged(TaggedAction),
}

/// Simple actions that carry no configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SimpleAction {
    /// Pre-admit the request as anonymous (skip SigV4).
    AllowAnonymous,
    /// Short-circuit with 403 Forbidden.
    Deny,
    /// Fall through to authentication (operator can use this as an
    /// explicit terminal for diagnostic visibility).
    Continue,
}

/// Actions that need a `type: X` tag because they carry fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case", tag = "type", deny_unknown_fields)]
pub enum TaggedAction {
    /// Return a custom HTTP status + body. Status must be 4xx or 5xx
    /// (validated at parse time). Intended for maintenance pages
    /// and rate-exceed errors.
    Reject {
        /// HTTP status code. Validated as 4xx/5xx.
        status: u16,
        /// Optional response body. `None` = empty body; caller can
        /// rely on the status code alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// Top-level admission section shape: an ordered list of blocks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSpec {
    /// Blocks evaluated in order. First matching block's action wins
    /// (RRR semantics).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<AdmissionBlockSpec>,
}

impl AdmissionSpec {
    /// Validate the full spec after deserialization. Called by the
    /// config loader so that semantic errors (duplicate block names,
    /// invalid Reject status, conflicting source IP forms) surface
    /// with precise file-position information via
    /// [`ConfigError::Parse`] rather than much later from the evaluator.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen_names = std::collections::HashSet::new();
        for block in &self.blocks {
            // Restrict block-name charset. Block names appear in
            // tracing::warn audit logs AND in the S3-style XML
            // AccessDenied body (`admission-deny:{name}`) the
            // middleware returns on `Deny`. XML injection via a
            // crafted name (e.g. `foo</Message><Fake>x`) is a real
            // risk — operators are trusted, but any admin-API abuse
            // chains into client response manipulation. Restrict to
            // a safe charset at parse time and reject the rest.
            if block.name.is_empty() {
                return Err(
                    "admission block name must not be empty — names surface in audit \
                     logs and client error bodies and need a non-empty identifier"
                        .to_string(),
                );
            }
            if block.name.len() > 128 {
                return Err(format!(
                    "admission block name `{}` exceeds 128 characters — keep names short \
                     for log readability",
                    block.name
                ));
            }
            if !block
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
            {
                return Err(format!(
                    "admission block name `{}` contains disallowed characters — allowed: \
                     ASCII alphanumerics and `-_:.`",
                    block.name
                ));
            }
            // Reserve the `public-prefix:` prefix for synthesized blocks
            // derived from `storage.buckets[*].public_prefixes`. An
            // operator authoring `name: public-prefix:releases` would
            // collide with the synthesized block for that bucket and
            // make audit logs / trace output ambiguous.
            if block.name.starts_with("public-prefix:") {
                return Err(format!(
                    "admission block name `{}` is reserved — the `public-prefix:` prefix is \
                     used for blocks synthesized from `storage.buckets[*].public_prefixes`. \
                     Pick a different name.",
                    block.name
                ));
            }

            if !seen_names.insert(block.name.as_str()) {
                return Err(format!(
                    "duplicate admission block name `{}` — block names must be unique \
                     across the chain",
                    block.name
                ));
            }
            block.match_.validate()?;
            block.action.validate()?;
        }
        Ok(())
    }
}

/// Maximum entries allowed in a single `source_ip_list`. The
/// evaluator does a linear scan per request, so an unbounded list is
/// a DoS knob. 4096 is ~2 orders of magnitude above any legitimate
/// IP denylist; operators who need more have likely outgrown
/// admission-chain-based IP gating and should front the proxy with
/// a dedicated WAF / IP-blocklist layer.
const MAX_SOURCE_IP_LIST: usize = 4096;

impl MatchSpec {
    /// Cross-field validation. `source_ip` is mutually exclusive with
    /// `source_ip_list` — operator must pick one form.
    fn validate(&self) -> Result<(), String> {
        if self.source_ip.is_some() && self.source_ip_list.is_some() {
            return Err(
                "match: `source_ip` and `source_ip_list` are mutually exclusive — use \
                 `source_ip_list` for multi-entry denylists and `source_ip` for a single IP"
                    .to_string(),
            );
        }
        if let Some(list) = &self.source_ip_list {
            if list.len() > MAX_SOURCE_IP_LIST {
                return Err(format!(
                    "match: `source_ip_list` has {} entries — cap is {}. The evaluator scans \
                     the list per request; lists that large belong in a dedicated WAF / \
                     IP-blocklist layer in front of the proxy.",
                    list.len(),
                    MAX_SOURCE_IP_LIST
                ));
            }
        }
        // Normalised method vec: uppercase and check for known methods.
        if let Some(methods) = &self.method {
            if methods.is_empty() {
                return Err(
                    "match: `method: []` means no methods — either omit the field to \
                     match any method, or list at least one"
                        .to_string(),
                );
            }
            for m in methods {
                let upper = m.to_ascii_uppercase();
                if !matches!(
                    upper.as_str(),
                    "GET" | "HEAD" | "PUT" | "POST" | "DELETE" | "PATCH" | "OPTIONS"
                ) {
                    return Err(format!(
                        "match: unknown HTTP method `{}` — expected GET/HEAD/PUT/POST/DELETE/PATCH/OPTIONS",
                        m
                    ));
                }
            }
        }
        // Compile-check the glob at parse time so syntactically-bad
        // patterns surface during `config lint` / `/apply` rather than
        // much later during chain build (where the block would be
        // silently skipped with only a `tracing::warn!`, breaking
        // operator-intended ordering).
        if let Some(glob) = &self.path_glob {
            if let Err(e) = globset::Glob::new(glob) {
                return Err(format!("match: invalid path_glob `{}`: {}", glob, e));
            }
        }
        Ok(())
    }
}

impl ActionSpec {
    fn validate(&self) -> Result<(), String> {
        match self {
            ActionSpec::Simple(_) => Ok(()),
            ActionSpec::Tagged(TaggedAction::Reject { status, message: _ }) => {
                if !(400..600).contains(status) {
                    return Err(format!(
                        "action: reject status `{}` must be 4xx or 5xx — Reject is for \
                         client/server errors, not successes or redirects",
                        status
                    ));
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_deny_simple_action() {
        let yaml = r#"
name: deny-bad-ips
match:
  source_ip_list: ["203.0.113.5", "198.51.100.0/24"]
action: deny
"#;
        let block: AdmissionBlockSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(block.name, "deny-bad-ips");
        assert!(matches!(
            block.action,
            ActionSpec::Simple(SimpleAction::Deny)
        ));
        let ips = block.match_.source_ip_list.as_ref().unwrap();
        assert_eq!(ips.len(), 2);
        // Bare IP promoted to /32.
        assert_eq!(ips[0].net.to_string(), "203.0.113.5/32");
        // CIDR preserved.
        assert_eq!(ips[1].net.to_string(), "198.51.100.0/24");
    }

    #[test]
    fn deserialize_reject_tagged_action() {
        let yaml = r#"
name: maint
match: {}
action:
  type: reject
  status: 503
  message: "try again"
"#;
        let block: AdmissionBlockSpec = serde_yaml::from_str(yaml).unwrap();
        match &block.action {
            ActionSpec::Tagged(TaggedAction::Reject { status, message }) => {
                assert_eq!(*status, 503);
                assert_eq!(message.as_deref(), Some("try again"));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn reject_2xx_status_is_error() {
        let yaml = r#"
name: bad
match: {}
action:
  type: reject
  status: 200
"#;
        let block: AdmissionBlockSpec = serde_yaml::from_str(yaml).unwrap();
        let err = block.action.validate().unwrap_err();
        assert!(err.contains("4xx") || err.contains("5xx"));
    }

    #[test]
    fn reject_without_message_deserializes() {
        let yaml = r#"
name: bare
match: {}
action:
  type: reject
  status: 429
"#;
        let block: AdmissionBlockSpec = serde_yaml::from_str(yaml).unwrap();
        match &block.action {
            ActionSpec::Tagged(TaggedAction::Reject { status, message }) => {
                assert_eq!(*status, 429);
                assert!(message.is_none());
            }
            _ => panic!(),
        }
    }

    #[test]
    fn match_conflicting_source_ip_forms_is_error() {
        let m = MatchSpec {
            source_ip: Some("203.0.113.5".parse().unwrap()),
            source_ip_list: Some(vec![]),
            ..Default::default()
        };
        let err = m.validate().unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn match_empty_method_list_is_error() {
        let m = MatchSpec {
            method: Some(vec![]),
            ..Default::default()
        };
        let err = m.validate().unwrap_err();
        assert!(err.contains("no methods"));
    }

    #[test]
    fn match_unknown_method_is_error() {
        let m = MatchSpec {
            method: Some(vec!["CONNECT".into()]),
            ..Default::default()
        };
        let err = m.validate().unwrap_err();
        assert!(err.contains("CONNECT"));
    }

    #[test]
    fn match_invalid_glob_is_error_at_parse_time() {
        // H1 from deep correctness review: syntactically-bad globs must
        // fail at validate() (lint / load / apply time), not silently
        // skip at chain-build time where operator-intended ordering is
        // already compromised.
        let m = MatchSpec {
            path_glob: Some("[invalid".into()),
            ..Default::default()
        };
        let err = m.validate().unwrap_err();
        assert!(
            err.contains("path_glob") && err.contains("invalid"),
            "error must name field + input, got: {err}"
        );
    }

    #[test]
    fn match_source_ip_list_overlimit_is_error() {
        // M3 from deep correctness review: an unbounded source_ip_list
        // is a DoS knob (evaluator does a linear scan per request).
        // Cap is 4096.
        let list: Vec<SourceIpEntry> = (0..5000)
            .map(|i| {
                let octet = (i % 256) as u8;
                SourceIpEntry::from_net(format!("203.0.113.{octet}/32").parse().unwrap())
            })
            .collect();
        let m = MatchSpec {
            source_ip_list: Some(list),
            ..Default::default()
        };
        let err = m.validate().unwrap_err();
        assert!(
            err.contains("source_ip_list") && err.contains("4096"),
            "error must name the field + the cap, got: {err}"
        );
    }

    #[test]
    fn admission_spec_reserved_public_prefix_name_is_error() {
        // H3 from deep correctness review: `public-prefix:*` is reserved
        // for synthesized blocks from `storage.buckets[*].public_prefixes`.
        // An operator authoring that prefix creates a name collision in
        // the runtime chain.
        let spec = AdmissionSpec {
            blocks: vec![AdmissionBlockSpec {
                name: "public-prefix:releases".into(),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
        };
        let err = spec.validate().unwrap_err();
        assert!(
            err.contains("reserved"),
            "error must explain the reservation, got: {err}"
        );
    }

    #[test]
    fn source_ip_entry_ipv6_parses() {
        let entry: SourceIpEntry = serde_yaml::from_str("\"2001:db8::1\"").unwrap();
        assert_eq!(entry.net.to_string(), "2001:db8::1/128");
    }

    #[test]
    fn source_ip_entry_invalid_is_error() {
        let err: Result<SourceIpEntry, _> = serde_yaml::from_str("\"not-an-ip\"");
        let e = err.unwrap_err();
        assert!(format!("{e}").contains("invalid IP or CIDR"));
    }

    #[test]
    fn admission_spec_block_name_rejects_xml_hostile_chars() {
        // Adversarial review H2: block names flow into the S3-style
        // XML `<Message>admission-deny:{name}</Message>` response
        // body. A crafted name with `<` or `>` would enable response-
        // splitting style payloads. Charset is restricted at parse.
        let spec = AdmissionSpec {
            blocks: vec![AdmissionBlockSpec {
                name: "hostile</Message><Fake>".into(),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
        };
        let err = spec.validate().unwrap_err();
        assert!(err.contains("disallowed"));
    }

    #[test]
    fn admission_spec_block_name_rejects_empty() {
        let spec = AdmissionSpec {
            blocks: vec![AdmissionBlockSpec {
                name: "".into(),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
        };
        let err = spec.validate().unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn admission_spec_block_name_rejects_overlong() {
        let spec = AdmissionSpec {
            blocks: vec![AdmissionBlockSpec {
                name: "a".repeat(200),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
        };
        let err = spec.validate().unwrap_err();
        assert!(err.contains("128"));
    }

    #[test]
    fn admission_spec_block_name_accepts_safe_charset() {
        let spec = AdmissionSpec {
            blocks: vec![AdmissionBlockSpec {
                name: "ok-name_v2:sub.0".into(),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
        };
        spec.validate().unwrap();
    }

    #[test]
    fn admission_spec_duplicate_block_names_is_error() {
        let spec = AdmissionSpec {
            blocks: vec![
                AdmissionBlockSpec {
                    name: "same".into(),
                    match_: MatchSpec::default(),
                    action: ActionSpec::Simple(SimpleAction::Continue),
                },
                AdmissionBlockSpec {
                    name: "same".into(),
                    match_: MatchSpec::default(),
                    action: ActionSpec::Simple(SimpleAction::Deny),
                },
            ],
        };
        let err = spec.validate().unwrap_err();
        assert!(err.contains("duplicate"));
    }

    #[test]
    fn admission_spec_roundtrips_through_yaml() {
        let spec = AdmissionSpec {
            blocks: vec![
                AdmissionBlockSpec {
                    name: "deny-bad".into(),
                    match_: MatchSpec {
                        source_ip_list: Some(vec![SourceIpEntry::from_net(
                            "203.0.113.0/24".parse().unwrap(),
                        )]),
                        ..Default::default()
                    },
                    action: ActionSpec::Simple(SimpleAction::Deny),
                },
                AdmissionBlockSpec {
                    name: "maint".into(),
                    match_: MatchSpec {
                        config_flag: Some("maintenance_mode".into()),
                        ..Default::default()
                    },
                    action: ActionSpec::Tagged(TaggedAction::Reject {
                        status: 503,
                        message: Some("back soon".into()),
                    }),
                },
            ],
        };
        let yaml = serde_yaml::to_string(&spec).unwrap();
        let back: AdmissionSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            spec, back,
            "spec must round-trip losslessly, emitted:\n{yaml}"
        );
    }

    #[test]
    fn match_with_bucket_and_path_glob() {
        let yaml = r#"
name: allow-releases
match:
  method: [GET, HEAD]
  bucket: releases
  path_glob: "*.zip"
action: allow-anonymous
"#;
        let block: AdmissionBlockSpec = serde_yaml::from_str(yaml).unwrap();
        let m = &block.match_;
        assert_eq!(
            m.method.as_deref(),
            Some(&["GET".to_string(), "HEAD".to_string()][..])
        );
        assert_eq!(m.bucket.as_deref(), Some("releases"));
        assert_eq!(m.path_glob.as_deref(), Some("*.zip"));
    }

    #[test]
    fn unknown_field_in_match_is_error() {
        // deny_unknown_fields catches typos like `source_ips` (plural).
        let yaml = r#"
name: oops
match:
  source_ips: ["203.0.113.5"]
action: deny
"#;
        let err: Result<AdmissionBlockSpec, _> = serde_yaml::from_str(yaml);
        assert!(err.is_err(), "typo in match field must fail parse");
    }

    #[test]
    fn source_ip_entry_preserves_explicit_slash_32() {
        // H2 from adversarial review: an operator authoring
        // `203.0.113.5/32` must NOT see it rewritten to `203.0.113.5`
        // on the next export — GitOps diffs would flip back and
        // forth on every apply.
        let entry: SourceIpEntry = serde_yaml::from_str("\"203.0.113.5/32\"").unwrap();
        let emitted = serde_yaml::to_string(&entry).unwrap();
        assert!(
            emitted.contains("203.0.113.5/32"),
            "/32 must survive round-trip, got: {emitted}"
        );
    }

    #[test]
    fn source_ip_entry_preserves_bare_ip_verbatim() {
        // Symmetric: bare IP stays bare on re-emit.
        let entry: SourceIpEntry = serde_yaml::from_str("\"203.0.113.5\"").unwrap();
        let emitted = serde_yaml::to_string(&entry).unwrap();
        assert!(
            emitted.contains("203.0.113.5") && !emitted.contains("/32"),
            "bare IP must not gain a /32 suffix, got: {emitted}"
        );
    }

    #[test]
    fn source_ip_entry_from_net_emits_canonical() {
        // Programmatic construction (tests, future GUI persistence)
        // goes through the canonical display — the operator's authored
        // form is the exception path.
        let entry = SourceIpEntry::from_net("203.0.113.0/24".parse().unwrap());
        let emitted = serde_yaml::to_string(&entry).unwrap();
        assert!(emitted.contains("203.0.113.0/24"));
    }

    #[test]
    fn source_ip_entry_equality_is_semantic() {
        // Two entries authored differently but with the same underlying
        // network must compare equal. The raw string only matters for
        // round-trip diffing.
        let a: SourceIpEntry = serde_yaml::from_str("\"203.0.113.5\"").unwrap();
        let b: SourceIpEntry = serde_yaml::from_str("\"203.0.113.5/32\"").unwrap();
        assert_eq!(a, b);
    }
}
