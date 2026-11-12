// SPDX-License-Identifier: BUSL-1.1

//! `Config::check` and its fatal / advisory siblings, plus the document
//! validation step (`validate_document`, `rule_gates`) that the admin API
//! and `config lint` share.

use super::*;

/// Why a config document is refused on its own, before any running config
/// is involved. [`Config::validate_document`] returns it; each caller words
/// it for its surface.
#[derive(Debug)]
pub enum DocumentRefusal {
    /// An empty or whitespace-only document: it would reset every field to
    /// its default, which is almost always a template or pipeline mistake.
    Empty,
    /// Shape or semantic parse error (unknown field, bad value, admission
    /// spec, shorthand conflict).
    Parse(ConfigError),
    /// `log_level` is not a tracing `EnvFilter` (the value in the file).
    LogFilter(String),
    /// Fatal `check_all` errors: the ones boot refuses too.
    Fatal(Vec<String>),
}

/// Which changed-only rule gate refused a write (see [`Config::rule_gates`]).
#[derive(Debug)]
pub enum RuleGateRefusal {
    Lifecycle(Vec<String>),
    Replication(Vec<String>),
    /// Declarative IAM the reconcile would refuse (review A13).
    DeclarativeIam(Vec<String>),
}

impl RuleGateRefusal {
    pub fn errors(&self) -> &[String] {
        match self {
            Self::Lifecycle(e) | Self::Replication(e) | Self::DeclarativeIam(e) => e,
        }
    }

    /// The rule family, for the refusal line.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Lifecycle(_) => "lifecycle",
            Self::Replication(_) => "replication",
            Self::DeclarativeIam(_) => "declarative IAM",
        }
    }
}

impl Config {
    /// THE document validation step, on the already-expanded text:
    /// `POST /config/validate`, `POST /config/apply` and `config lint` all
    /// run it. Returns the config and its advisory warnings.
    pub fn validate_document(text: &str) -> Result<(Config, Vec<String>), DocumentRefusal> {
        if text.trim().is_empty() {
            return Err(DocumentRefusal::Empty);
        }
        // The dual-shape loader: the legacy flat shape or the sectioned one.
        let mut cfg = Config::from_yaml_str(text).map_err(DocumentRefusal::Parse)?;
        // A malformed filter must not enter the runtime config and then fail
        // at the next restart.
        if cfg
            .log_level
            .parse::<tracing_subscriber::EnvFilter>()
            .is_err()
        {
            return Err(DocumentRefusal::LogFilter(cfg.log_level));
        }
        // The document is not env-applied here, but its advisories judge it
        // with the environment it runs under (DGP_TRUST_PROXY_HEADERS).
        cfg.tuning = super::RuntimeTuning::from_env(&super::process_env);
        let warnings = cfg.check_all().map_err(DocumentRefusal::Fatal)?;
        Ok((cfg, warnings))
    }

    /// The changed-only rule gates of a write from `old` to `self`: a fatal
    /// lifecycle rule error (a delete rule without `expire_after`, …) and a
    /// duplicate replication rule name refuse the write only when the write
    /// changes that block. An error on UNCHANGED lifecycle content comes back
    /// as a standing warning, so a pre-existing bad rule cannot block an
    /// unrelated edit. `config lint` passes a default `old`: every rule
    /// counts as changed.
    pub fn rule_gates(&self, old: &Config) -> Result<Vec<String>, RuleGateRefusal> {
        let mut standing =
            crate::lifecycle::planner::lifecycle_gate(&old.lifecycle, &self.lifecycle)
                .map_err(RuleGateRefusal::Lifecycle)?;
        // State, cursor and lease are keyed by rule name (#13).
        crate::config_sections::replication_gate(
            &old.replication,
            &self.replication,
            &|b| old.storage_key_of(b),
            &|b| self.storage_key_of(b),
        )
        .map_err(RuleGateRefusal::Replication)?;
        standing.extend(self.declarative_iam_gate(old)?);
        Ok(standing)
    }

    /// The I/O-free half of the declarative reconcile's validation (review
    /// A13: `config lint` passed a YAML the boot then refused). A change the
    /// reconcile will apply is refused like the apply refuses it; unchanged
    /// declarative IAM (a node that booted with a contained rule) only warns,
    /// so an unrelated edit is not blocked. Gui-mode YAML IAM is inert.
    fn declarative_iam_gate(&self, old: &Config) -> Result<Vec<String>, RuleGateRefusal> {
        use crate::iam::permissions::ConditionPolicy;
        if !matches!(self.iam_mode, crate::config_sections::IamMode::Declarative) {
            return Ok(Vec::new());
        }
        let policy = if self.declarative_reconcile_needed(old) {
            ConditionPolicy::Refuse
        } else {
            ConditionPolicy::Contain
        };
        let yaml = crate::iam::snapshot_from_access(
            &self.iam_users,
            &self.iam_groups,
            &self.auth_providers,
            &self.group_mapping_rules,
            &[],
        );
        crate::iam::declarative::validate_declared(&yaml, policy)
            .map_err(|e| RuleGateRefusal::DeclarativeIam(vec![e]))
    }

    /// True when a change from `old` must run the declarative reconcile:
    /// the target mode is declarative and the IAM fields (or the mode)
    /// changed. The one rule of the write gates and the transition.
    pub fn declarative_reconcile_needed(&self, old: &Config) -> bool {
        use crate::config_sections::IamMode;
        if !matches!(self.iam_mode, IamMode::Declarative) {
            return false;
        }
        let old_iam_unchanged = matches!(old.iam_mode, IamMode::Declarative)
            && old.iam_users == self.iam_users
            && old.iam_groups == self.iam_groups
            && old.auth_providers == self.auth_providers
            && old.group_mapping_rules == self.group_mapping_rules;
        !old_iam_unchanged
    }

    /// Check the config for problems. Returns a list of human-readable
    /// warnings; also clears fields that cannot be satisfied (currently just
    /// unresolvable `default_backend`).
    ///
    /// Single source of truth for config validation. The startup path calls
    /// [`Self::validate`] which is a thin wrapper that logs each warning to
    /// stderr; the admin API calls `check` directly to return warnings as
    /// structured data.
    pub(super) fn check(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        // NaN and infinity are valid YAML float literals (`.nan` / `.inf`) but
        // break the downstream ratio test: NaN comparisons are always false, so
        // NaN silently disables delta compression; INFINITY > 1.0 is true so a
        // naive warning fires, but the value survives and causes every file to
        // be stored as a delta regardless of size. Clamp both to the default
        // so neither can corrupt compression decisions.
        if self.max_delta_ratio.is_nan() {
            warnings.push("max_delta_ratio is NaN — replacing with default 0.75".to_string());
            self.max_delta_ratio = default_max_delta_ratio();
        } else if self.max_delta_ratio.is_infinite() {
            warnings.push("max_delta_ratio is infinite — replacing with default 0.75".to_string());
            self.max_delta_ratio = default_max_delta_ratio();
        } else if self.max_delta_ratio < 0.0 || self.max_delta_ratio > 1.0 {
            warnings.push(format!(
                "max_delta_ratio={} is outside [0.0, 1.0] — delta compression decisions may behave unexpectedly",
                self.max_delta_ratio
            ));
        }
        // The checks below are about a value the operator set. An `auto`
        // value is derived from the spool (see `limits::derive_limits`), and
        // its warnings come from there.
        if let limits::BytesOrAuto::Bytes(max_object_size) = self.max_object_size {
            if max_object_size == 0 {
                warnings.push("max_object_size=0 will reject all uploads".to_string());
            }
            // A PUT body is held in memory (its signature is checked over it),
            // then stored with ONE backend request, and S3 stores at most 5 GiB
            // in one request: a larger body is buffered only to be refused.
            const SINGLE_PUT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;
            if max_object_size > SINGLE_PUT_MAX_BYTES {
                warnings.push(format!(
                    "max_object_size={max_object_size} is above 5 GiB ({SINGLE_PUT_MAX_BYTES} bytes), the most \
                     that an S3 backend stores with one PUT request: the proxy holds such a body \
                     in memory and then the backend refuses it. Set it to 5 GiB or less; larger \
                     objects arrive as multipart uploads"
                ));
            }
        }
        if let Some(resolved) = &self.tuning.limits {
            warnings.extend(resolved.warning_messages());
        }
        // Duplicate backend names and routes to undefined backends are FATAL,
        // not warnings: see `check_fatal`, which `check_all` runs first.

        if let Some(ref default) = self.default_backend {
            if !self.backends.is_empty() && !self.backends.iter().any(|b| &b.name == default) {
                warnings.push(format!(
                    "default_backend='{}' not found in backends list {:?} — clearing",
                    default,
                    self.backends.iter().map(|b| &b.name).collect::<Vec<_>>()
                ));
                self.default_backend = None;
            }
        }
        // `alias` is applied by the routing table only together with an
        // explicit `backend` (the real name lives on THAT backend); without
        // one the bucket is served under its own name and the alias does
        // nothing. Say so instead of ignoring it silently.
        for (bucket, policy) in &self.buckets {
            if let (Some(alias), None) = (policy.alias.as_deref(), policy.backend.as_deref()) {
                if alias != bucket {
                    warnings.push(format!(
                        "bucket '{bucket}': alias '{alias}' is ignored because the policy \
                         has no `backend` — set `backend` to the backend that holds \
                         '{alias}', or remove the alias"
                    ));
                }
            }
        }
        // replication_target_only coherence. The marker's safety argument is
        // "replication is the SINGLE writer"; warn on configs that weaken it.
        // See docs/product/how-to/backend-capability-validation.md.
        let eq_bucket = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        for (bucket, policy) in &self.buckets {
            if !policy.replication_target_only {
                continue;
            }
            if !self
                .replication
                .rules
                .iter()
                .any(|r| eq_bucket(&r.destination.bucket, bucket))
            {
                warnings.push(format!(
                    "bucket '{bucket}' is configured for replication targets only but no replication rule \
                     targets it — writes are blocked and nothing replicates in. Add a rule \
                     or remove the marker."
                ));
            }
            for rule in &self.lifecycle.rules {
                let writes_into_marked = eq_bucket(&rule.bucket, bucket)
                    || matches!(
                        &rule.action,
                        crate::config_sections::LifecycleAction::Transition(t)
                            if eq_bucket(&t.destination.bucket, bucket)
                    );
                if writes_into_marked {
                    warnings.push(format!(
                        "lifecycle rule '{}' writes into bucket '{bucket}' which is configured \
                         for replication targets only — a second internal writer weakens the single-writer \
                         guarantee on non-CAS backends.",
                        rule.name
                    ));
                }
            }
        }
        // Aliasing hole: a marked bucket's single-writer guarantee applies to
        // its REAL (backend, bucket) storage; an unmarked second virtual name
        // resolving to the same real location reopens client writes to it.
        {
            let resolved: Vec<(&String, StorageKey, bool)> = self
                .buckets
                .iter()
                .map(|(name, p)| (name, self.storage_key_of(name), p.replication_target_only))
                .collect();
            for (name, key, marked) in &resolved {
                if !marked {
                    continue;
                }
                for (other, okey, omarked) in &resolved {
                    if other != name && !omarked && okey == key {
                        warnings.push(format!(
                            "bucket '{other}' aliases the same storage as bucket '{name}' which is \
                             configured for replication targets only, but '{other}' is NOT marked — client \
                             writes through '{other}' defeat the single-writer guarantee. Mark every virtual \
                             name that points at the protected storage."
                        ));
                    }
                }
            }
        }
        // Two replication rules writing overlapping prefixes of one destination
        // bucket = two writers into the same deltaspace. Warn regardless of the
        // marker — the safety argument is per destination prefix.
        let rules = &self.replication.rules;
        for i in 0..rules.len() {
            for j in (i + 1)..rules.len() {
                let (a, b) = (&rules[i], &rules[j]);
                let (ap, bp) = (
                    crate::replication::normalize_prefix(&a.destination.prefix),
                    crate::replication::normalize_prefix(&b.destination.prefix),
                );
                let same_storage = self.storage_key_of(&a.destination.bucket)
                    == self.storage_key_of(&b.destination.bucket);
                if same_storage && (ap.starts_with(&bp) || bp.starts_with(&ap)) {
                    warnings.push(format!(
                        "replication rules '{}' and '{}' both write into the storage {} \
                         (buckets '{}' and '{}') with overlapping prefixes — give each rule \
                         a distinct destination prefix.",
                        a.name,
                        b.name,
                        self.storage_key_of(&a.destination.bucket),
                        a.destination.bucket,
                        b.destination.bucket,
                    ));
                }
            }
        }

        // Per-backend encryption validation. Each named backend + the
        // legacy singleton gets checked against:
        //   * native modes (SseKms / SseS3) on filesystem backends → error.
        //   * Aes256GcmProxy with no key configured (after env-var
        //     resolution) → warning: the engine refuses every write to
        //     that backend (`keyless_proxy_backends`). Not fatal: `config
        //     lint` has no deploy env, where the key often comes from.
        //   * key_id charset: must match `[A-Za-z0-9_.-]{1,64}`.
        //   * collisions: two backends declaring the same `key_id`
        //     but different `key` bytes → error.
        let mut explicit_key_ids: std::collections::BTreeMap<
            String,
            Vec<(String, Option<String>)>,
        > = std::collections::BTreeMap::new();
        let mut validate_entry = |label: &str,
                                  backend: &BackendConfig,
                                  enc: &BackendEncryptionConfig,
                                  warnings: &mut Vec<String>| {
            if matches!(backend, BackendConfig::Filesystem { .. })
                && matches!(
                    enc,
                    BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. }
                )
            {
                warnings.push(format!(
                    "backend '{}' uses a native S3 encryption mode ({}) on a filesystem \
                     backend — native modes require S3. Change mode to 'aes256-gcm-proxy' \
                     or 'none'.",
                    label,
                    enc.mode_tag()
                ));
            }
            // The shim decrypts with the retired KEY; its id alone reads nothing.
            if enc.legacy_key_id().is_some() && enc.legacy_key().is_none() {
                warnings.push(format!(
                    "backend '{}' sets encryption.legacy_key_id but no legacy_key — \
                     objects written with that key stay unreadable. Add the retired key \
                     as legacy_key, or remove legacy_key_id.",
                    label
                ));
            }
            if let BackendEncryptionConfig::Aes256GcmProxy {
                key,
                key_id,
                legacy_key: _,
                legacy_key_id: _,
            } = enc
            {
                if key.is_none() {
                    warnings.push(format!(
                        "backend '{}' uses aes256-gcm-proxy but no key is configured. \
                         Unless the env var {} supplies it, the proxy refuses every write \
                         to this backend (it never stores plaintext there).",
                        label,
                        if label == "default" {
                            "DGP_ENCRYPTION_KEY".to_string()
                        } else {
                            format!(
                                "DGP_BACKEND_{}_ENCRYPTION_KEY",
                                env_suffix_for_backend_name(label)
                            )
                        }
                    ));
                }
                if let Some(kid) = key_id {
                    if !is_valid_key_id(kid) {
                        warnings.push(format!(
                            "backend '{}' has encryption.key_id='{}' — must match \
                             [A-Za-z0-9_.-]{{1,64}} (S3 user-metadata header-safe).",
                            label, kid
                        ));
                    }
                    if let Some(k) = key {
                        explicit_key_ids
                            .entry(kid.clone())
                            .or_default()
                            .push((label.to_string(), Some(k.clone())));
                    }
                }
            }
        };
        validate_entry(
            "default",
            &self.backend,
            &self.backend_encryption,
            &mut warnings,
        );
        for named in &self.backends {
            validate_entry(
                &named.name,
                &named.backend,
                &named.encryption,
                &mut warnings,
            );
        }
        // Cross-backend: same explicit key_id but different keys.
        for (kid, entries) in &explicit_key_ids {
            if entries.len() > 1 {
                let distinct_keys: std::collections::BTreeSet<&Option<String>> =
                    entries.iter().map(|(_, k)| k).collect();
                if distinct_keys.len() > 1 {
                    let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
                    warnings.push(format!(
                        "backends {:?} share key_id='{}' but declare DIFFERENT keys. \
                         Either make the keys identical (intentional cross-backend portability) \
                         or give each backend a distinct key_id.",
                        names, kid
                    ));
                }
            }
        }

        // Replication rules — static validation + cycle detection.
        // Catches operator errors at config load time so the worker
        // never has to deal with malformed rules at runtime.
        warnings.extend(crate::config_sections::validate_replication(
            &self.replication,
        ));
        // Fatal in `rule_gates` (on change); a warning here so a config that
        // already holds one still loads. The worker refuses to run the rule.
        warnings.extend(crate::config_sections::replication_scope_overlap_errors(
            &self.replication.rules,
            &|b| self.storage_key_of(b),
        ));
        warnings.extend(crate::config_sections::validate_lifecycle(&self.lifecycle));
        warnings.extend(self.bucket_encryption_warnings());
        warnings.extend(crate::config_sections::validate_jobs(&self.jobs));
        warnings.extend(crate::config_sections::validate_event_delivery(
            &self.event_delivery,
        ));

        // Cross-field advisories — "this combination is suspicious" checks that a
        // single field can't reveal (rate-limit/trust-proxy collapse, stale IAM
        // templates, etc). Non-fatal; rendered alongside the warnings above.
        let env = advisories::EnvView::of(self);
        warnings.extend(
            advisories::advisories(self, &env)
                .iter()
                .map(|a| a.render()),
        );

        warnings
    }

    /// Run `Self::check` and log each warning to stderr. Used by the
    /// startup path where eprintln is the right sink.
    pub fn validate(&mut self) {
        for warning in self.check() {
            eprintln!("Warning: {}", warning);
        }
    }

    /// THE validation entry point for every surface that judges a config
    /// before it runs (`config lint`, `/config/validate`, section
    /// validate/apply). `Err` carries the fatal errors that boot and apply
    /// refuse; `Ok` carries the warnings. Running both here keeps lint and
    /// validate from passing a config the server rejects.
    pub fn check_all(&mut self) -> Result<Vec<String>, Vec<String>> {
        let fatal = self.check_fatal();
        if !fatal.is_empty() {
            return Err(fatal);
        }
        Ok(self.check())
    }

    /// FATAL config errors — graph states the proxy must never run with,
    /// as opposed to `Self::check`'s advisory warnings:
    ///
    ///   * a bucket routed to an UNDEFINED backend name — requests would
    ///     silently fall through to the default backend and 404/misroute
    ///     (the beshu-b2 incident);
    ///   * duplicate backend names — ambiguous routing, "first entry wins"
    ///     is a silent coin-flip after a config edit.
    ///
    /// Enforced at boot (refuse to start) and at every apply / section-PUT
    /// (reject the transition). Pure read — never mutates.
    pub fn check_fatal(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for backend in &self.backends {
            if !seen.insert(backend.name.as_str()) {
                errors.push(format!(
                    "duplicate backend name '{}' — backend names must be unique",
                    backend.name
                ));
            }
        }
        for (bucket, policy) in &self.buckets {
            if let Some(ref backend) = policy.backend {
                // "default" is the SYNTHESIZED name of the singleton
                // `storage.backend` (what the GUI displays and the admin API
                // accepts). It exists only while `backends[]` is empty; with
                // named backends, `RoutingBackend::new` refuses it.
                let routable = if self.backends.is_empty() {
                    backend == "default"
                } else {
                    self.backends.iter().any(|b| &b.name == backend)
                };
                if !routable {
                    errors.push(format!(
                        "bucket '{bucket}' routes to undefined backend '{backend}' — \
                         define the backend under storage.backends or remove the route \
                         (available: {:?})",
                        self.backends.iter().map(|b| &b.name).collect::<Vec<_>>()
                    ));
                }
            }
        }
        errors.extend(self.zero_tunable_errors());
        errors.extend(self.coordination_bucket_exposures());
        errors.extend(self.bucket_encryption_errors());
        errors.extend(self.kms_key_id_errors());
        errors
    }

    /// The S3 backends whose endpoint HOST is a name, as `(name, host,
    /// kind)`. With `previous`, only the backends whose definition is new or
    /// changed against it. Pure.
    pub(crate) fn resolution_targets(
        &self,
        previous: Option<&Config>,
        env_allow_local: bool,
    ) -> Vec<(String, String, crate::security::UrlKind)> {
        let mut out = Vec::new();
        for (name, backend) in self.effective_backends() {
            if previous.is_some_and(|old| old.backend_by_name(name) == Some(backend)) {
                continue;
            }
            let BackendConfig::S3 {
                endpoint: Some(ep),
                allow_local,
                ..
            } = backend
            else {
                continue;
            };
            let Some(host) = reqwest::Url::parse(ep)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
            else {
                continue;
            };
            let host = host.trim_matches(['[', ']']).to_string();
            if host.parse::<std::net::IpAddr>().is_ok() {
                continue; // a literal address: the text check judged it
            }
            out.push((
                name.to_string(),
                host,
                crate::storage::s3_endpoint_kind_with(*allow_local, env_allow_local),
            ));
        }
        out
    }

    /// FATAL errors of the S3 backends whose endpoint HOST resolves only to
    /// addresses that the connect-time guard refuses (a name that points at
    /// this host's loopback without `allow_local`, or at a link-local or
    /// metadata address). Every request to such a backend would fail, so
    /// boot and apply refuse the config and name the backend and the
    /// setting. A defined backend that no bucket uses counts too. All the
    /// lookups run at once under one deadline of 5 s; a name that does not
    /// resolve in time says nothing: DNS may come up later. `previous`: an
    /// apply judges only the backends it adds or changes. Needs I/O, so it is
    /// apart from [`Self::check_fatal`].
    pub async fn backend_resolution_errors(&self, previous: Option<&Config>) -> Vec<String> {
        self.backend_resolution_errors_with(
            previous,
            crate::config::env_bool("DGP_BACKEND_ALLOW_LOCAL", false),
        )
        .await
    }

    /// [`Self::backend_resolution_errors`] with the value of
    /// `DGP_BACKEND_ALLOW_LOCAL` given by the caller.
    pub(crate) async fn backend_resolution_errors_with(
        &self,
        previous: Option<&Config>,
        env_allow_local: bool,
    ) -> Vec<String> {
        let lookups = self
            .resolution_targets(previous, env_allow_local)
            .into_iter()
            .map(|(name, host, kind)| async move {
                let found = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::net::lookup_host((host.as_str(), 0)),
                )
                .await;
                let Ok(Ok(addrs)) = found else { return None };
                let ips: Vec<std::net::IpAddr> = addrs.map(|sa| sa.ip()).collect();
                crate::security::backend_resolution_error(&host, &ips, kind).map(|e| {
                    format!(
                        "backend '{name}': endpoint host '{host}' (backend setting \
                         `allow_local`, env DGP_BACKEND_ALLOW_LOCAL): {e}"
                    )
                })
            });
        futures::future::join_all(lookups)
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// `advanced` count settings where 0 stops the proxy: `blocking_threads: 0`
    /// panics tokio at the next start (a crash loop after a good-looking
    /// apply), `codec_concurrency: 0` answers every delta PUT with 503, and
    /// `filtered_list_max_engine_pages: 0` fails every filtered LIST. Unset
    /// means the default; only an explicit 0 is refused.
    fn zero_tunable_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let mut refuse = |name: &str, what: &str| {
            errors.push(format!(
                "advanced.{name} is 0: {what}. Use at least 1, or remove the setting for the default"
            ));
        };
        if self.blocking_threads == Some(0) {
            refuse(
                "blocking_threads",
                "the runtime cannot start with no blocking thread",
            );
        }
        if self.codec_concurrency == Some(0) {
            refuse(
                "codec_concurrency",
                "no delta encode or decode could ever run",
            );
        }
        if self.filtered_list_max_engine_pages == 0 {
            refuse(
                "filtered_list_max_engine_pages",
                "every filtered LIST would fail at once",
            );
        }
        errors
    }

    /// Backends that the engine wraps (the singleton `default` only while
    /// no named backends exist) whose mode is `aes256-gcm-proxy` with no
    /// key, after the `DGP_*` env overrides. The engine refuses every write
    /// to them (`WriteMode::Refuse`) so that they never store plaintext.
    pub fn keyless_proxy_backends(&self) -> Vec<String> {
        let keyless = |enc: &BackendEncryptionConfig| {
            matches!(
                enc,
                BackendEncryptionConfig::Aes256GcmProxy { key: None, .. }
            )
        };
        if self.backends.is_empty() {
            return if keyless(&self.backend_encryption) {
                vec!["default".to_string()]
            } else {
                Vec::new()
            };
        }
        self.backends
            .iter()
            .filter(|b| keyless(&b.encryption))
            .map(|b| b.name.clone())
            .collect()
    }

    /// The apply gate for [`Self::keyless_proxy_backends`]: an error per
    /// backend that this transition leaves in `aes256-gcm-proxy` mode with
    /// no key while `old` did not. A backend that was keyless already (a
    /// running config from before this rule) does not block an unrelated
    /// apply; its writes stay refused at runtime. Not part of
    /// [`Self::check_fatal`]: `config lint` runs without the deploy env, and
    /// the key of a valid config often comes from `DGP_*_ENCRYPTION_KEY`.
    pub fn newly_keyless_proxy_backends(&self, old: &Config) -> Vec<String> {
        let before = old.keyless_proxy_backends();
        let mut out: Vec<String> = self
            .keyless_proxy_backends()
            .into_iter()
            .filter(|name| !before.contains(name))
            .map(|name| {
                let (env, _) = backend_encryption_env_names(&name);
                format!(
                    "backend '{name}' uses aes256-gcm-proxy but has no key: the proxy would \
                     refuse every write to it rather than store plaintext. Set \
                     encryption.key or {env}, or choose another mode"
                )
            })
            .collect();
        // The same rule for a bucket override (issue #116). A bucket key has
        // no DGP_* variable: it comes from the YAML or a `${env:NAME}` ref.
        let before = old.keyless_proxy_buckets();
        out.extend(
            self.keyless_proxy_buckets()
                .into_iter()
                .filter(|name| !before.contains(name))
                .map(|name| {
                    format!(
                        "bucket '{name}' uses aes256-gcm-proxy but has no key: the proxy would \
                         refuse every write to it rather than store plaintext. Set the \
                         bucket's encryption.key (a literal or a ${{env:NAME}} reference), or \
                         choose another mode"
                    )
                }),
        );
        out
    }

    /// Ways the config would make the coordination bucket
    /// (`config_sync_bucket`) reachable as client data: public read, an
    /// alias from or onto it, or a replication/lifecycle rule that reads or
    /// writes it. A `backend:` route alone is fine: it only says which
    /// backend hosts the bucket (see [`Self::coordination_backend`]).
    pub(super) fn coordination_bucket_exposures(&self) -> Vec<String> {
        let Some(sync) = self
            .config_sync_bucket
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        else {
            return Vec::new();
        };
        let is_sync = |b: &str| b.trim().eq_ignore_ascii_case(sync);
        let mut errors = Vec::new();
        let why = "it is reserved for the proxy's config sync, leases and locks";
        for (bucket, policy) in &self.buckets {
            let alias_hits = policy.alias.as_deref().is_some_and(is_sync);
            if is_sync(bucket) {
                if policy.public == Some(true) || !policy.public_prefixes.is_empty() {
                    errors.push(format!(
                        "bucket '{bucket}' is the coordination bucket (config_sync_bucket) and \
                         cannot be public: {why}"
                    ));
                }
                if policy.alias.as_deref().is_some_and(|a| !is_sync(a)) {
                    errors.push(format!(
                        "bucket '{bucket}' is the coordination bucket (config_sync_bucket) and \
                         cannot have an alias: {why}"
                    ));
                }
            } else if alias_hits {
                errors.push(format!(
                    "bucket '{bucket}' has alias '{sync}', the coordination bucket \
                     (config_sync_bucket): {why}"
                ));
            }
        }
        for rule in &self.replication.rules {
            if is_sync(&rule.source.bucket) || is_sync(&rule.destination.bucket) {
                errors.push(format!(
                    "replication rule '{}' uses the coordination bucket '{sync}' \
                     (config_sync_bucket): {why}",
                    rule.name
                ));
            }
        }
        for rule in &self.lifecycle.rules {
            if crate::lifecycle::planner::rule_write_buckets(rule)
                .into_iter()
                .any(is_sync)
            {
                errors.push(format!(
                    "lifecycle rule '{}' uses the coordination bucket '{sync}' \
                     (config_sync_bucket): {why}",
                    rule.name
                ));
            }
        }
        errors
    }
}

#[cfg(test)]
mod object_size_tests {
    use super::*;

    /// A PUT is stored with one backend request, and S3 takes at most
    /// 5 GiB in one request: a larger `max_object_size` only lets the proxy
    /// buffer a body that the backend then refuses. `config check` says so.
    #[test]
    fn a_max_object_size_over_5_gib_warns() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let warns = |size: u64| {
            let mut c = Config {
                max_object_size: limits::BytesOrAuto::Bytes(size),
                ..Config::default()
            };
            c.check()
                .into_iter()
                .any(|w| w.contains("max_object_size") && w.contains("5 GiB"))
        };
        assert!(warns(5 * GIB + 1));
        assert!(!warns(5 * GIB));
        assert!(!warns(Config::default().max_object_size_bytes()));
    }

    /// An `auto` value is never an operator's choice: no 5 GiB warning,
    /// however large the spool makes it (the limit of a backend PUT is a
    /// separate item).
    #[test]
    fn an_auto_max_object_size_does_not_warn_about_5_gib() {
        let mut c = Config::default();
        let facts = limits::DiskFacts {
            free: 100 * limits::GIB,
            total: 200 * limits::GIB,
            own_spool_bytes: 0,
            is_tmpfs: false,
            shares_device_with_data: false,
            in_kubernetes: false,
        };
        c.tuning.limits = Some(limits::ResolvedLimits {
            effective: limits::derive_limits(facts, Default::default()).0,
            warnings: vec![],
            dir: "/spool".into(),
            facts,
            env_set: vec![],
        });
        assert!(c.max_object_size_bytes() > 5 * limits::GIB);
        assert!(!c.check().iter().any(|w| w.contains("5 GiB")));
    }

    /// The warnings of the derivation reach `Config::check` (boot, lint,
    /// apply): a rule that nothing lists warns nobody.
    #[test]
    fn check_carries_the_limit_warnings() {
        let mut c = Config::default();
        let facts = limits::DiskFacts {
            free: 100 * limits::GIB,
            total: 200 * limits::GIB,
            own_spool_bytes: 0,
            is_tmpfs: false,
            shares_device_with_data: false,
            in_kubernetes: false,
        };
        let (effective, warnings) = limits::derive_limits(
            facts,
            limits::ExplicitLimits {
                spool: limits::SpoolSetting::Bytes(99 * limits::GIB),
                ..Default::default()
            },
        );
        assert!(!warnings.is_empty());
        c.tuning.limits = Some(limits::ResolvedLimits {
            effective,
            warnings,
            dir: "/spool".into(),
            facts,
            env_set: vec![],
        });
        assert!(c
            .check()
            .iter()
            .any(|w| w.contains("DGP_SPOOL_MAX_BYTES") && w.contains("/spool")));
    }
}

#[cfg(test)]
mod backend_resolution_tests {
    use super::*;

    fn s3(endpoint: &str, allow_local: bool) -> BackendConfig {
        BackendConfig::S3 {
            endpoint: Some(endpoint.into()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: None,
            secret_access_key: None,
            allow_local,
            session_token: None,
        }
    }

    /// `localhost` resolves to loopback: a backend on it is refused up
    /// front, with the backend name and the setting, unless `allow_local`.
    #[tokio::test]
    async fn a_backend_host_that_resolves_to_loopback_is_refused_at_check_time() {
        let mut cfg = Config {
            backend: s3("https://localhost:9000", false),
            ..Default::default()
        };
        let errors = cfg.backend_resolution_errors_with(None, false).await;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("backend 'default'")
                && errors[0].contains("localhost")
                && errors[0].contains("allow_local"),
            "{}",
            errors[0]
        );
        cfg.backend = s3("https://localhost:9000", true);
        assert!(cfg
            .backend_resolution_errors_with(None, false)
            .await
            .is_empty());
        // A literal address is judged by the text check, not here.
        cfg.backend = s3("https://127.0.0.1:9000", false);
        assert!(cfg
            .backend_resolution_errors_with(None, false)
            .await
            .is_empty());
        cfg.backend = BackendConfig::Filesystem {
            path: "/data".into(),
        };
        assert!(cfg
            .backend_resolution_errors_with(None, false)
            .await
            .is_empty());
    }

    /// An apply judges only the backends it adds or changes; the boot judges
    /// every defined backend, also one that no bucket uses.
    #[test]
    fn an_apply_judges_only_the_backends_it_changes() {
        let named = |name: &str, ep: &str| NamedBackendConfig {
            name: name.into(),
            backend: s3(ep, false),
            encryption: Default::default(),
        };
        let old = Config {
            backends: vec![named("hetzner-fsn1", "https://s3.example.org")],
            ..Default::default()
        };
        let same = old.clone();
        assert!(same.resolution_targets(Some(&old), false).is_empty());
        let added = Config {
            backends: vec![
                named("hetzner-fsn1", "https://s3.example.org"),
                named("aws-dr", "https://dr.example.org"),
            ],
            ..Default::default()
        };
        let targets = added.resolution_targets(Some(&old), false);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "aws-dr");
        // The boot has no previous config: every backend with a host name.
        assert_eq!(added.resolution_targets(None, false).len(), 2);
    }

    /// The two call sites are the rule: remove the boot call or the apply
    /// call and an unreachable backend runs again.
    #[test]
    fn boot_and_apply_both_run_the_resolution_check() {
        let main = crate::source_scan::prod_text(&crate::source_scan::read("src/main.rs"));
        assert!(
            main.contains("config.backend_resolution_errors(None)"),
            "the boot must refuse a backend host that resolves only to refused addresses"
        );
        let apply = crate::source_scan::prod_text(&crate::source_scan::read(
            "src/api/admin/config/transition.rs",
        ));
        assert!(
            apply.contains("new_cfg.backend_resolution_errors(Some(old_cfg))"),
            "an apply must refuse a new or changed backend host that resolves only to refused addresses"
        );
    }
}
