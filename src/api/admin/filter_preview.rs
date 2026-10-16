// SPDX-License-Identifier: BUSL-1.1

//! `POST /_/api/admin/jobs/filter-preview`: what a rule's include/exclude
//! globs do to the keys of a bucket, before the rule is saved.
//!
//! The answer comes from the same matcher the rules run
//! ([`crate::glob_filter`]) on a sample of real keys: the first `limit`
//! keys under the prefix (or the `keys` the request names, for "test a
//! key"). Each glob reports how many sampled keys it matches, a glob that
//! does not compile reports its error, and a glob that matches nothing gets
//! a suggestion when the prefix in front of it would match: globs match the
//! whole key from the bucket root, which is the commonest surprise.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use globset::GlobSet;
use serde::{Deserialize, Serialize};

use super::extract::AdminJson;
use super::path_guard::{AdminBucket, AdminObjectPath};
use super::{AdminError, AdminState};
use crate::glob_filter::{self, Verdict};

/// The most keys one preview reads under the prefix. A first-N sample
/// lied: keys sort by name, so a glob for later names (`*-pre*` builds)
/// matched nothing in it. Counting every key up to this cap costs one
/// LIST request per 1,000 keys, once per cache period.
const SCAN_CAP: usize = 200_000;
/// Keys of each verdict returned for display; the counts cover them all.
pub const KEYS_PER_VERDICT: usize = 500;
/// The most keys one request may name.
const MAX_NAMED_KEYS: usize = 100;
/// How long a scan is reused: an editor previews on every keystroke.
const SAMPLE_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
pub struct FilterPreviewRequest {
    pub bucket: AdminBucket,
    #[serde(default)]
    pub prefix: AdminObjectPath,
    #[serde(default)]
    pub include_globs: Vec<String>,
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Classify these keys instead of a sample of the bucket.
    #[serde(default)]
    pub keys: Option<Vec<AdminObjectPath>>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyVerdict {
    Included,
    Excluded,
    NotIncluded,
    /// A proxy-internal key (`.deltaglider/`): no rule acts on it.
    Internal,
}

#[derive(Debug, Serialize)]
pub struct PreviewKey {
    pub key: String,
    pub verdict: KeyVerdict,
    /// The glob that decided, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GlobReport {
    pub glob: String,
    /// `include` or `exclude`.
    pub list: &'static str,
    /// Sampled keys that the glob matches (whether or not it decided).
    pub matches: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For a glob that matches nothing: the same glob with the prefix in
    /// front, when that matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FilterPreviewResponse {
    /// Keys classified: every key under the prefix, up to the scan cap.
    pub sampled: usize,
    /// More keys exist under the prefix than the scan cap.
    pub truncated: bool,
    pub included: usize,
    pub excluded: usize,
    pub not_included: usize,
    pub internal: usize,
    pub globs: Vec<GlobReport>,
    /// At most [`KEYS_PER_VERDICT`] keys of each verdict, in key order.
    pub keys: Vec<PreviewKey>,
}

/// One list of globs: the set of the ones that compile, and per glob its
/// index in that set or its error.
struct GlobList {
    set: GlobSet,
    entries: Vec<(String, Result<usize, String>)>,
}

impl GlobList {
    fn compile(globs: &[String]) -> Self {
        let mut valid = Vec::new();
        let entries = globs
            .iter()
            .map(|g| match glob_filter::compile(std::slice::from_ref(g)) {
                Ok(_) => {
                    valid.push(g.clone());
                    (g.clone(), Ok(valid.len() - 1))
                }
                Err(e) => (g.clone(), Err(e.reason)),
            })
            .collect();
        // Each one compiled alone, so the set compiles.
        let set = glob_filter::compile(&valid).unwrap_or_else(|_| GlobSet::empty());
        Self { set, entries }
    }

    /// The glob at `index` of the set.
    fn name(&self, index: usize) -> Option<String> {
        self.entries
            .iter()
            .find(|(_, i)| *i == Ok(index))
            .map(|(g, _)| g.clone())
    }
}

/// Pure: the preview of `include`/`exclude` over `keys` under `prefix`.
pub fn preview(
    prefix: &str,
    include: &[String],
    exclude: &[String],
    keys: &[String],
    truncated: bool,
) -> FilterPreviewResponse {
    let inc = GlobList::compile(include);
    let exc = GlobList::compile(exclude);
    let mut inc_hits = vec![0usize; inc.entries.len()];
    let mut exc_hits = vec![0usize; exc.entries.len()];
    let mut out = FilterPreviewResponse {
        sampled: keys.len(),
        truncated,
        included: 0,
        excluded: 0,
        not_included: 0,
        internal: 0,
        globs: Vec::new(),
        keys: Vec::new(),
    };
    for key in keys {
        count_hits(&inc, key, &mut inc_hits);
        count_hits(&exc, key, &mut exc_hits);
        let (verdict, glob) = if crate::replication::planner::is_internal_key(key) {
            (KeyVerdict::Internal, None)
        } else {
            match glob_filter::verdict(key, &inc.set, &exc.set) {
                Verdict::Included(by) => (KeyVerdict::Included, by.and_then(|i| inc.name(i))),
                Verdict::Excluded(i) => (KeyVerdict::Excluded, exc.name(i)),
                Verdict::NotIncluded => (KeyVerdict::NotIncluded, None),
            }
        };
        let n = match verdict {
            KeyVerdict::Included => &mut out.included,
            KeyVerdict::Excluded => &mut out.excluded,
            KeyVerdict::NotIncluded => &mut out.not_included,
            KeyVerdict::Internal => &mut out.internal,
        };
        *n += 1;
        if *n > KEYS_PER_VERDICT {
            continue;
        }
        out.keys.push(PreviewKey {
            key: key.clone(),
            verdict,
            glob,
        });
    }
    for (list, globs, hits) in [("include", &inc, &inc_hits), ("exclude", &exc, &exc_hits)] {
        for (pos, (glob, compiled)) in globs.entries.iter().enumerate() {
            let matches = hits[pos];
            out.globs.push(GlobReport {
                glob: glob.clone(),
                list,
                matches,
                error: compiled.as_ref().err().cloned(),
                suggestion: if compiled.is_ok() && matches == 0 {
                    with_prefix_suggestion(prefix, glob, keys)
                } else {
                    None
                },
            });
        }
    }
    out
}

/// Count, per entry of `list`, whether it matches `key`.
fn count_hits(list: &GlobList, key: &str, hits: &mut [usize]) {
    let matched = list.set.matches(key);
    for (pos, (_, compiled)) in list.entries.iter().enumerate() {
        if let Ok(i) = compiled {
            if matched.contains(i) {
                hits[pos] += 1;
            }
        }
    }
}

/// `prefix` + `glob`, when the glob does not start with the prefix and the
/// joined glob matches a sampled key.
fn with_prefix_suggestion(prefix: &str, glob: &str, keys: &[String]) -> Option<String> {
    if prefix.is_empty() || glob.starts_with(prefix) || glob.starts_with("**") {
        return None;
    }
    let joined = format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        glob.trim_start_matches('/')
    );
    let set = glob_filter::compile(std::slice::from_ref(&joined)).ok()?;
    keys.iter().any(|k| set.is_match(k)).then_some(joined)
}

type SampleKey = (String, String, usize);
/// A scan: its keys, and whether more exist.
type Sample = Arc<(Vec<String>, bool)>;
type SampleCache = moka::future::Cache<SampleKey, Sample>;

/// A few scans (one per prefix being edited), single-flight: two previews
/// of one cold prefix list it once.
fn sample_cache() -> &'static SampleCache {
    static CACHE: OnceLock<SampleCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        moka::future::Cache::builder()
            .max_capacity(4)
            .time_to_live(SAMPLE_TTL)
            .build()
    })
}

/// POST /_/api/admin/jobs/filter-preview — admin session.
pub async fn filter_preview(
    State(state): State<Arc<AdminState>>,
    AdminJson(req): AdminJson<FilterPreviewRequest>,
) -> Result<Json<FilterPreviewResponse>, AdminError> {
    let bucket = req.bucket.admit(&state)?;
    let (keys, truncated) = match req.keys {
        Some(keys) => {
            if keys.len() > MAX_NAMED_KEYS {
                return Err(AdminError::invalid(format!(
                    "at most {MAX_NAMED_KEYS} keys per request"
                )));
            }
            let keys = keys.iter().map(|k| k.to_string()).collect();
            (Arc::new((keys, false)), false)
        }
        None => {
            let limit = req.limit.unwrap_or(SCAN_CAP).clamp(1, SCAN_CAP);
            let cache_key = (bucket.to_string(), req.prefix.to_string(), limit);
            let budget = state.config.read().await.filtered_list_max_engine_pages;
            let sample = sample_cache()
                .try_get_with(cache_key, async {
                    let engine = state.s3_state.engine.load();
                    crate::iam::listing::list_keys_for_caller(
                        &**engine,
                        &bucket,
                        &req.prefix,
                        None,
                        None,
                        limit,
                        budget,
                    )
                    .await
                    .map(Arc::new)
                })
                .await
                .map_err(|e| match &*e {
                    crate::iam::listing::ListingError::Engine(e) => {
                        AdminError::internal(e.to_string())
                    }
                    other => AdminError::invalid(format!("{other:?}")),
                })?;
            let truncated = sample.1;
            (sample, truncated)
        }
    };
    Ok(Json(preview(
        &req.prefix,
        &req.include_globs,
        &req.exclude_globs,
        &keys.0,
        truncated,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn each_key_gets_its_verdict_and_the_deciding_glob() {
        let keys = s(&[
            "ror/builds/a.zip",
            "ror/builds/a.tmp",
            "ror/notes.txt",
            ".deltaglider/state",
        ]);
        let p = preview(
            "ror/",
            &s(&["*.zip", "*.tmp"]),
            &s(&["*.tmp"]),
            &keys,
            false,
        );
        let verdicts: Vec<(&str, &KeyVerdict, Option<&str>)> = p
            .keys
            .iter()
            .map(|k| (k.key.as_str(), &k.verdict, k.glob.as_deref()))
            .collect();
        assert_eq!(
            verdicts,
            vec![
                ("ror/builds/a.zip", &KeyVerdict::Included, Some("*.zip")),
                ("ror/builds/a.tmp", &KeyVerdict::Excluded, Some("*.tmp")),
                ("ror/notes.txt", &KeyVerdict::NotIncluded, None),
                (".deltaglider/state", &KeyVerdict::Internal, None),
            ]
        );
        assert_eq!(
            (p.included, p.excluded, p.not_included, p.internal),
            (1, 1, 1, 1)
        );
        // The include `*.tmp` matches one key, though the exclude decided it.
        let tmp_include = p
            .globs
            .iter()
            .find(|g| g.list == "include" && g.glob == "*.tmp");
        assert_eq!(tmp_include.map(|g| g.matches), Some(1));
    }

    #[test]
    fn a_glob_without_the_prefix_gets_a_suggestion_and_a_bad_glob_its_error() {
        let keys = s(&["ror/builds/a.zip", "ror/builds/b.zip"]);
        let p = preview("ror/", &[], &s(&["builds/**", "bad["]), &keys, false);
        let builds = &p.globs[0];
        assert_eq!(builds.matches, 0);
        assert_eq!(builds.suggestion.as_deref(), Some("ror/builds/**"));
        let bad = &p.globs[1];
        assert!(bad.error.is_some());
        // The good glob still previews: nothing is excluded by it.
        assert_eq!(p.included, 2);
    }

    #[test]
    fn no_suggestion_when_the_glob_already_matches_or_starts_with_the_prefix() {
        let keys = s(&["ror/a.zip"]);
        let p = preview("ror/", &s(&["*.zip", "ror/x/**"]), &[], &keys, false);
        assert!(
            p.globs.iter().all(|g| g.suggestion.is_none()),
            "{:?}",
            p.globs
        );
    }
}
