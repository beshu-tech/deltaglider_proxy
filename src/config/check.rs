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
}

impl RuleGateRefusal {
    pub fn errors(&self) -> &[String] {
        match self {
            Self::Lifecycle(e) | Self::Replication(e) => e,
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
        let standing = crate::lifecycle::planner::lifecycle_gate(&old.lifecycle, &self.lifecycle)
            .map_err(RuleGateRefusal::Lifecycle)?;
        // State, cursor and lease are keyed by rule name (#13).
        crate::config_sections::replication_gate(&old.replication, &self.replication)
            .map_err(RuleGateRefusal::Replication)?;
        Ok(standing)
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
        if self.max_object_size == 0 {
            warnings.push("max_object_size=0 will reject all uploads".to_string());
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
            let resolved: Vec<(&String, String, String, bool)> = self
                .buckets
                .iter()
                .map(|(name, p)| {
                    (
                        name,
                        p.backend.clone().unwrap_or_default().to_ascii_lowercase(),
                        p.alias
                            .clone()
                            .unwrap_or_else(|| name.clone())
                            .to_ascii_lowercase(),
                        p.replication_target_only,
                    )
                })
                .collect();
            for (name, backend, real, marked) in &resolved {
                if !marked {
                    continue;
                }
                for (other, ob, oreal, omarked) in &resolved {
                    if other != name && !omarked && ob == backend && oreal == real {
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
                if eq_bucket(&a.destination.bucket, &b.destination.bucket)
                    && (a.destination.prefix.starts_with(&b.destination.prefix)
                        || b.destination.prefix.starts_with(&a.destination.prefix))
                {
                    warnings.push(format!(
                        "replication rules '{}' and '{}' both write into bucket '{}' with \
                         overlapping prefixes — give each rule a distinct destination prefix.",
                        a.name, b.name, a.destination.bucket
                    ));
                }
            }
        }

        // Per-backend encryption validation. Each named backend + the
        // legacy singleton gets checked against:
        //   * native modes (SseKms / SseS3) on filesystem backends → error.
        //   * Aes256GcmProxy with no key configured (after env-var
        //     resolution) → warning (key must come from env at runtime;
        //     this is informational so operators notice a missing
        //     DGP_*_ENCRYPTION_KEY before a read fails).
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
            if let BackendEncryptionConfig::Aes256GcmProxy {
                key,
                key_id,
                legacy_key: _,
                legacy_key_id: _,
            } = enc
            {
                if key.is_none() {
                    warnings.push(format!(
                        "backend '{}' uses aes256-gcm-proxy but no key is configured in \
                         YAML — set it via env var ({}).",
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

        // Env-var suffix collision check (correctness x-ray C4):
        // Two backends whose names normalise to the same env suffix
        // would both read the SAME env var
        // (`DGP_BACKEND_<SUFFIX>_ENCRYPTION_KEY`). When both land with
        // the same key material but different derived key_ids (because
        // the raw backend NAME feeds into `derive_key_id`, not the
        // suffix), objects written by one cannot be read by the other.
        // Detectable at config load time; warn loudly so operators
        // rename one.
        if self.backends.len() > 1 {
            let mut suffix_owners: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for backend in &self.backends {
                // Skip names that already normalize to the bare
                // "DEFAULT" suffix — that collides with the singleton
                // `DGP_ENCRYPTION_KEY` path, which is an independent
                // surface; the operator probably meant a DIFFERENT
                // collision warning which would confuse the signal.
                let suffix = env_suffix_for_backend_name(&backend.name);
                suffix_owners
                    .entry(suffix)
                    .or_default()
                    .push(backend.name.clone());
            }
            for (suffix, names) in &suffix_owners {
                if names.len() > 1 {
                    warnings.push(format!(
                        "backends {:?} normalize to the same env-var suffix '{}' — \
                         all would read DGP_BACKEND_{}_ENCRYPTION_KEY, but each has a \
                         distinct derived key_id. Rename one of them to avoid silent \
                         cross-backend key sharing (Aes256GcmProxy cross-decrypt will fail \
                         with 'key id mismatch').",
                        names, suffix, suffix
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
        warnings.extend(crate::config_sections::validate_lifecycle(&self.lifecycle));
        warnings.extend(crate::config_sections::validate_jobs(&self.jobs));
        warnings.extend(crate::config_sections::validate_event_delivery(
            &self.event_delivery,
        ));

        // Cross-field advisories — "this combination is suspicious" checks that a
        // single field can't reveal (rate-limit/trust-proxy collapse, stale IAM
        // templates, etc). Non-fatal; rendered alongside the warnings above.
        let env = advisories::EnvView::from_env();
        warnings.extend(
            advisories::advisories(self, &env)
                .iter()
                .map(|a| a.render()),
        );

        warnings
    }

    /// Run [`Self::check`] and log each warning to stderr. Used by the
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
    /// as opposed to [`Self::check`]'s advisory warnings:
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
        errors.extend(self.coordination_bucket_exposures());
        errors
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
