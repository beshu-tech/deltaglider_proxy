// SPDX-License-Identifier: BUSL-1.1

//! The admission chain, applied per RESOURCE.
//!
//! A `path_glob` decides KEYS, never request text. The middleware sees only
//! the request line, but a request can touch other resources: the source of
//! a CopyObject or UploadPartCopy, each key of a DeleteObjects body, the form
//! field key of a browser POST, every key and folder that a LIST returns.
//! [`ResourceRules`] carries the request-wide facts (source IP, signed or
//! not) and walks the chain again for each such resource, with the chain's
//! own first-match-wins order. A resource-level `deny` / `reject` refuses the
//! resource; an anonymous LIST shows only what an `allow-anonymous` block
//! admits.
//!
//! The `method` of a block names the EFFECT on the resource ([`Effect`]): a
//! copy source is read (GET), whatever the method of the request line.

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::Arc;

use super::evaluator::{
    bucket_matches, evaluate_blocks, request_facts_match, MethodView, RequestInfo,
};
use super::{Action, AdmissionBlock, AdmissionChain, Decision, Match, PUBLIC_PREFIX_BLOCK_PREFIX};

/// Why a resource is refused: the block that fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    Deny {
        matched: String,
    },
    Reject {
        matched: String,
        status: u16,
        message: Option<String>,
    },
}

impl Refusal {
    pub fn matched(&self) -> &str {
        match self {
            Self::Deny { matched } | Self::Reject { matched, .. } => matched,
        }
    }

    /// The HTTP status of the refusal: 403 for `deny`, the operator's for `reject`.
    pub fn status(&self) -> u16 {
        match self {
            Self::Deny { .. } => 403,
            Self::Reject { status, .. } => *status,
        }
    }

    /// The message of the refusal (the same text the request-line check gives).
    pub fn message(&self) -> String {
        match self {
            Self::Deny { matched } => format!("Blocked by access rule '{matched}'"),
            Self::Reject {
                matched, message, ..
            } => message
                .clone()
                .unwrap_or_else(|| format!("Rejected by access rule '{matched}'")),
        }
    }
}

/// What a request does to a resource that its request line does not name:
/// the meaning of a block's `method` for that resource. THE mapping:
///
/// | Resource | Effect | `method` that matches |
/// |---|---|---|
/// | a copy or part-copy source, a listed key, a download or ZIP entry | `Read` | `GET` or `HEAD` |
/// | a `DeleteObjects` key, a bulk delete key | `Delete` | `DELETE` |
/// | a form POST key, a bulk copy or move destination | `Write` | `PUT` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Read,
    Write,
    Delete,
}

impl Effect {
    /// The effect of an IAM action on a key (the admin bulk endpoints name
    /// their action). A listing reads; `Admin` names no key and counts as a write.
    pub fn of_action(action: crate::iam::S3Action) -> Self {
        use crate::iam::S3Action;
        match action {
            S3Action::Read | S3Action::List => Self::Read,
            S3Action::Write | S3Action::Admin => Self::Write,
            S3Action::Delete => Self::Delete,
        }
    }

    /// The method that stands for the effect in a rule walk.
    fn method(self) -> &'static str {
        match self {
            Self::Read => "GET",
            Self::Write => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// What a LIST does with one folder name (a CommonPrefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixVerdict {
    /// Every key below the name is refused (or not admitted): the name is hidden.
    Hidden,
    /// No key below the name is refused: the name is shown.
    Shown,
    /// Some keys below the name are refused: the name is shown only when a
    /// key that the caller may see exists below it.
    NeedsProof,
}

/// The chain and the request-wide facts of one request. Built by the
/// admission middleware (only when [`AdmissionChain::has_resource_rules`]),
/// carried in the request extensions, consulted at every point that
/// authorizes a resource next to IAM.
#[derive(Debug, Clone)]
pub struct ResourceRules {
    chain: Arc<AdmissionChain>,
    /// The method of the request line: a key that the request writes or
    /// deletes also matches a block that names it.
    line_method: String,
    source_ip: Option<IpAddr>,
    authenticated: bool,
    /// The request was admitted as anonymous by an operator block: it sees
    /// only the keys that an `allow-anonymous` block admits.
    anonymous_block: bool,
}

impl ResourceRules {
    /// `None` when no block decides per resource.
    pub fn new(
        chain: Arc<AdmissionChain>,
        line_method: &str,
        source_ip: Option<IpAddr>,
        authenticated: bool,
    ) -> Option<Self> {
        chain.has_resource_rules().then(|| Self {
            chain,
            line_method: line_method.to_ascii_uppercase(),
            source_ip,
            authenticated,
            anonymous_block: false,
        })
    }

    /// The request was admitted as anonymous by block `matched`. A
    /// synthesised `public-prefix:` block keeps its own rule (the bucket's
    /// public prefixes), so only an operator block narrows the listing.
    pub fn admitted_anonymously_by(mut self, matched: &str) -> Self {
        self.anonymous_block = !matched.starts_with(PUBLIC_PREFIX_BLOCK_PREFIX);
        self
    }

    /// The bucket name, lower case (S3 bucket names are case-insensitive):
    /// borrowed when it already is, which it nearly always is.
    fn lowered(bucket: &str) -> Cow<'_, str> {
        if bucket.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(bucket.to_ascii_lowercase())
        } else {
            Cow::Borrowed(bucket)
        }
    }

    /// Which methods a block's `method` meets for a resource with `effect`.
    fn view(&self, effect: Effect) -> MethodView<'_> {
        match effect {
            Effect::Read => MethodView::Read,
            Effect::Write | Effect::Delete => MethodView::Mutation {
                line: &self.line_method,
            },
        }
    }

    /// The rules for the resources of ONE bucket: the name is lowered once
    /// and the blocks that can concern the bucket are picked once, in chain
    /// order, so a loop over many keys does neither per key.
    pub fn in_bucket<'a>(&'a self, bucket: &'a str) -> BucketRules<'a> {
        let bucket = Self::lowered(bucket);
        let blocks = self
            .chain
            .blocks()
            .iter()
            .filter(|b| block_concerns_bucket(b, &bucket))
            .collect();
        BucketRules {
            rules: self,
            bucket,
            blocks,
        }
    }

    /// THE check of a resource beyond the request line: is `bucket/key`,
    /// which the request reads, writes or deletes (`effect`), refused by a
    /// `deny` / `reject` block? (One resource; a loop uses [`Self::in_bucket`].)
    pub fn refusal(&self, effect: Effect, bucket: &str, key: &str) -> Option<Refusal> {
        self.in_bucket(bucket).refusal(effect, key)
    }

    /// Is a LIST of `bucket` under `prefix` refused outright? See [`BucketRules::list_refusal`].
    pub fn list_refusal(&self, bucket: &str, prefix: &str) -> Option<Refusal> {
        self.in_bucket(bucket).list_refusal(prefix)
    }

    /// May a LIST show `bucket/key`? See [`BucketRules::lists_key`].
    pub fn lists_key(&self, bucket: &str, key: &str) -> bool {
        self.in_bucket(bucket).lists_key(key)
    }

    /// Can a block change what a LIST of `bucket` shows?
    pub fn affects_bucket(&self, bucket: &str) -> bool {
        if self.anonymous_block {
            return true;
        }
        let bucket = Self::lowered(bucket);
        self.chain.blocks().iter().any(|b| {
            b.decides_per_resource()
                && matches!(&b.match_, Match::Predicates(p) if p.bucket.as_ref().is_none_or(|x| *x == *bucket))
        })
    }

    /// The folder verdict of a LIST. See [`BucketRules::prefix_verdict`].
    pub fn prefix_verdict(&self, bucket: &str, prefix: &str) -> PrefixVerdict {
        self.in_bucket(bucket).prefix_verdict(prefix)
    }
}

/// Can `block` concern a resource of `bucket` (lower case)? A block with
/// another bucket never does, so the walk skips it.
fn block_concerns_bucket(block: &AdmissionBlock, bucket: &str) -> bool {
    match &block.match_ {
        Match::Predicates(p) => p.bucket.as_ref().is_none_or(|b| b == bucket),
        Match::PublicPrefixGrant { bucket: b } => b == bucket,
    }
}

/// The [`ResourceRules`] of one bucket ([`ResourceRules::in_bucket`]).
pub struct BucketRules<'a> {
    rules: &'a ResourceRules,
    /// Lower case.
    bucket: Cow<'a, str>,
    /// The blocks that can concern the bucket, in chain order.
    blocks: Vec<&'a AdmissionBlock>,
}

impl BucketRules<'_> {
    /// How many blocks can concern the bucket.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    fn info<'b>(&'b self, effect: Effect, key: Option<&'b str>) -> RequestInfo<'b> {
        RequestInfo {
            method: effect.method(),
            bucket: &self.bucket,
            key,
            list_prefix: None,
            authenticated: self.rules.authenticated,
            source_ip: self.rules.source_ip,
        }
    }

    fn decide(&self, effect: Effect, key: Option<&str>, prefix: Option<&str>) -> Decision {
        let mut info = self.info(effect, key);
        info.list_prefix = prefix;
        evaluate_blocks(
            &self.rules.chain,
            self.blocks.iter().copied(),
            &info,
            self.rules.view(effect),
        )
    }

    /// Is `key`, which the request reads, writes or deletes (`effect`),
    /// refused by a `deny` / `reject` block?
    pub fn refusal(&self, effect: Effect, key: &str) -> Option<Refusal> {
        refusal_of(self.decide(effect, Some(key), None))
    }

    /// Is a LIST under `prefix` refused outright? A `deny` or `reject` block
    /// whose glob covers every key below the prefix refuses it, as it would
    /// the S3 request (the keyless rule of the evaluator).
    pub fn list_refusal(&self, prefix: &str) -> Option<Refusal> {
        refusal_of(self.decide(Effect::Read, None, Some(prefix)))
    }

    /// May a LIST of this request show `key`? Never a key that a `deny`
    /// covers. A `reject` hides nothing: a maintenance rule must not make a
    /// sync client delete its copies; the key's GET still gets the rule's
    /// status. For an anonymous request admitted by an operator block, only
    /// a key that an `allow-anonymous` block admits.
    pub fn lists_key(&self, key: &str) -> bool {
        match self.decide(Effect::Read, Some(key), None) {
            Decision::Deny { .. } => false,
            Decision::Reject { .. } | Decision::AllowAnonymous { .. } => true,
            Decision::Continue { .. } => !self.rules.anonymous_block,
        }
    }

    /// May a key directly in `folder` be refused by a `deny` or `reject`
    /// block for a read? The delta facts of an object depend on the other
    /// objects of its folder (a compression oracle on them), so a writer or
    /// reader gets them only when this is `false`. The chain order is
    /// ignored: an earlier carve-out can only make the answer too careful.
    pub fn may_refuse_in_folder(&self, folder: &str) -> bool {
        let req = self.info(Effect::Read, None);
        self.blocks.iter().any(|block| {
            let Match::Predicates(p) = &block.match_ else {
                return false;
            };
            matches!(block.action, Action::Deny | Action::Reject { .. })
                && request_facts_match(p, &block.action, &req, MethodView::Read)
                && bucket_matches(p, &req)
                && p.path_glob
                    .as_ref()
                    .is_none_or(|g| g.can_match_in_folder(folder))
        })
    }

    /// THE rule for a folder name of a LIST: hidden only when every key
    /// below it is necessarily refused (or, for an anonymous request, not
    /// admitted), shown when none can be, and otherwise shown only when a
    /// visible key exists below it (the caller proves that by reading keys).
    /// The chain is walked in order for the whole set of keys that start
    /// with `prefix`: a block either decides all of them (its glob matches
    /// every key below) or some.
    pub fn prefix_verdict(&self, prefix: &str) -> PrefixVerdict {
        let req = self.info(Effect::Read, None);
        let (mut some_hidden, mut some_shown) = (false, false);
        let anonymous_block = self.rules.anonymous_block;
        let verdict = |hides: bool, some_hidden: bool, some_shown: bool| match (
            hides,
            some_hidden,
            some_shown,
        ) {
            (true, _, false) => PrefixVerdict::Hidden,
            (false, false, _) => PrefixVerdict::Shown,
            _ => PrefixVerdict::NeedsProof,
        };
        for block in self.blocks.iter().copied() {
            let p = match &block.match_ {
                Match::Predicates(p) => p,
                Match::PublicPrefixGrant { .. } => {
                    // May admit some keys of an anonymous GET/HEAD.
                    if !self.rules.authenticated {
                        some_shown = true;
                    }
                    continue;
                }
            };
            if !request_facts_match(p, &block.action, &req, MethodView::Read)
                || !bucket_matches(p, &req)
            {
                continue;
            }
            let all = match &p.path_glob {
                None => true,
                Some(g) if !g.can_match_below(prefix) => continue,
                Some(g) => g.matches_all_below(prefix),
            };
            // A `reject` hides nothing from a listing (see `lists_key`).
            let hides = match block.action {
                Action::Deny => true,
                Action::Reject { .. } | Action::AllowAnonymous => false,
                Action::Continue => anonymous_block,
            };
            if all {
                return verdict(hides, some_hidden, some_shown);
            }
            if hides {
                some_hidden = true;
            } else {
                some_shown = true;
            }
        }
        verdict(anonymous_block, some_hidden, some_shown)
    }
}

/// The refusal that a decision is, if it is one.
fn refusal_of(decision: Decision) -> Option<Refusal> {
    match decision {
        Decision::Deny { matched } => Some(Refusal::Deny { matched }),
        Decision::Reject {
            matched,
            status,
            message,
        } => Some(Refusal::Reject {
            matched,
            status,
            message,
        }),
        Decision::AllowAnonymous { .. } | Decision::Continue { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::spec::{
        ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction, TaggedAction,
    };
    use std::collections::BTreeMap;

    fn block(
        name: &str,
        bucket: Option<&str>,
        glob: Option<&str>,
        action: ActionSpec,
    ) -> AdmissionBlockSpec {
        AdmissionBlockSpec {
            name: name.into(),
            match_: MatchSpec {
                bucket: bucket.map(Into::into),
                path_glob: glob.map(Into::into),
                ..Default::default()
            },
            action,
        }
    }

    fn deny() -> ActionSpec {
        ActionSpec::Simple(SimpleAction::Deny)
    }
    fn cont() -> ActionSpec {
        ActionSpec::Simple(SimpleAction::Continue)
    }
    fn anon() -> ActionSpec {
        ActionSpec::Simple(SimpleAction::AllowAnonymous)
    }

    fn rules(blocks: Vec<AdmissionBlockSpec>, authenticated: bool) -> ResourceRules {
        rules_for_line(blocks, "GET", authenticated)
    }

    fn rules_for_line(
        blocks: Vec<AdmissionBlockSpec>,
        line_method: &str,
        authenticated: bool,
    ) -> ResourceRules {
        let chain = Arc::new(AdmissionChain::from_config_parts(&BTreeMap::new(), &blocks));
        ResourceRules::new(chain, line_method, None, authenticated).expect("a resource rule")
    }

    #[test]
    fn a_chain_with_no_resource_rule_makes_no_rules() {
        let chain = Arc::new(AdmissionChain::from_config_parts(
            &BTreeMap::new(),
            &[
                block("by-ip", None, None, deny()),
                block("whole-bucket-anon", Some("b"), None, anon()),
            ],
        ));
        assert!(ResourceRules::new(chain, "GET", None, true).is_none());
    }

    #[test]
    fn a_deny_with_a_glob_refuses_the_resource_it_names() {
        let r = rules(
            vec![block(
                "no-secret",
                Some("releases"),
                Some("secret/**"),
                deny(),
            )],
            true,
        );
        assert_eq!(
            r.refusal(Effect::Read, "releases", "secret/s1.txt"),
            Some(Refusal::Deny {
                matched: "no-secret".into()
            })
        );
        assert_eq!(
            r.refusal(Effect::Read, "releases", "team-b/copied.txt"),
            None
        );
        // Another bucket is another resource.
        assert_eq!(r.refusal(Effect::Read, "other", "secret/s1.txt"), None);
        // The bucket name is compared in lower case, as for the request line.
        assert!(r
            .refusal(Effect::Read, "Releases", "secret/s1.txt")
            .is_some());
    }

    /// A `bucket`-only deny applies to a resource in that bucket although
    /// the request line names another (a copy out of it).
    #[test]
    fn a_bucket_only_deny_applies_to_a_source_in_that_bucket() {
        let r = rules(vec![block("no-vault", Some("vault"), None, deny())], true);
        assert!(r.refusal(Effect::Read, "vault", "any").is_some());
        assert!(r.refusal(Effect::Read, "open", "any").is_none());
    }

    /// First match wins per resource: an earlier `continue` for the same
    /// resource ends the walk before the deny.
    #[test]
    fn the_chain_order_decides_per_resource() {
        let early_continue = vec![
            block("carve-out", Some("b"), Some("secret/public/**"), cont()),
            block("no-secret", Some("b"), Some("secret/**"), deny()),
        ];
        let r = rules(early_continue, true);
        assert!(r.refusal(Effect::Read, "b", "secret/public/x").is_none());
        assert!(r.refusal(Effect::Read, "b", "secret/private/x").is_some());
        // Deny first: the later continue never runs.
        let deny_first = vec![
            block("no-secret", Some("b"), Some("secret/**"), deny()),
            block("carve-out", Some("b"), Some("secret/public/**"), cont()),
        ];
        let r = rules(deny_first, true);
        assert!(r.refusal(Effect::Read, "b", "secret/public/x").is_some());
    }

    /// `method` names the EFFECT on the resource: a copy source is read
    /// (GET, or a block that names HEAD), a DeleteObjects key is deleted, a
    /// form POST key is written. For a key that the request writes or deletes
    /// a block also matches the method of the request line (`POST`).
    #[test]
    fn method_is_the_effect_on_the_resource() {
        let with = |methods: &[&str]| {
            let mut b = block("no-secret", Some("b"), Some("secret/**"), deny());
            b.match_.method = Some(methods.iter().map(|m| m.to_string()).collect());
            // The request line of DeleteObjects and of a form POST is POST.
            rules_for_line(vec![b], "POST", true)
        };
        let refused = |r: &ResourceRules, e: Effect| r.refusal(e, "b", "secret/x").is_some();
        // GET: a read only.
        let r = with(&["GET"]);
        assert!(refused(&r, Effect::Read));
        assert!(!refused(&r, Effect::Delete) && !refused(&r, Effect::Write));
        // HEAD: a read matches a block that names HEAD too.
        let r = with(&["HEAD"]);
        assert!(refused(&r, Effect::Read));
        assert!(!refused(&r, Effect::Delete) && !refused(&r, Effect::Write));
        // DELETE: a delete only.
        let r = with(&["DELETE"]);
        assert!(refused(&r, Effect::Delete));
        assert!(!refused(&r, Effect::Read) && !refused(&r, Effect::Write));
        // PUT: a write only.
        let r = with(&["PUT"]);
        assert!(refused(&r, Effect::Write));
        assert!(!refused(&r, Effect::Read) && !refused(&r, Effect::Delete));
        // POST is the request line of a DeleteObjects and of a form POST: it
        // covers the keys that the request writes or deletes, never a read.
        let r = with(&["POST"]);
        assert!(refused(&r, Effect::Write) && refused(&r, Effect::Delete));
        assert!(!refused(&r, Effect::Read));
        // A list of methods matches any of its effects.
        let r = with(&["GET", "DELETE"]);
        assert!(refused(&r, Effect::Read) && refused(&r, Effect::Delete));
        assert!(!refused(&r, Effect::Write));
        // The request line is no effect of a READ source: a copy (line PUT)
        // reads its source, so a PUT rule does not cover the source.
        let mut b = block("no-secret", Some("b"), Some("secret/**"), deny());
        b.match_.method = Some(vec!["PUT".into()]);
        let r = rules_for_line(vec![b], "PUT", true);
        assert!(!refused(&r, Effect::Read));
        // The listing of keys is a read.
        assert!(!with(&["GET"]).lists_key("b", "secret/x"));
        assert!(with(&["PUT"]).lists_key("b", "secret/x"));
        // The effect of an IAM action.
        use crate::iam::S3Action;
        assert_eq!(Effect::of_action(S3Action::List), Effect::Read);
        assert_eq!(Effect::of_action(S3Action::Read), Effect::Read);
        assert_eq!(Effect::of_action(S3Action::Write), Effect::Write);
        assert_eq!(Effect::of_action(S3Action::Delete), Effect::Delete);

        // The other conditions describe the request as a whole.
        let mut signed_only = block("signed", Some("b"), Some("secret/**"), deny());
        signed_only.match_.authenticated = Some(true);
        assert!(rules(vec![signed_only.clone()], true)
            .refusal(Effect::Read, "b", "secret/x")
            .is_some());
        assert!(rules(vec![signed_only], false)
            .refusal(Effect::Read, "b", "secret/x")
            .is_none());
    }

    fn with_methods(methods: &[&str]) -> AdmissionBlockSpec {
        let mut b = block("no-mutation", None, None, deny());
        b.match_.method = Some(methods.iter().map(|m| m.to_string()).collect());
        b.match_.source_ip = Some("10.1.2.3".parse().unwrap());
        b
    }

    /// A block with only `method` and a network decides per resource too:
    /// it covers each DeleteObjects key and the form POST key.
    #[test]
    fn a_method_only_deny_covers_the_keys_the_request_deletes_or_writes() {
        let chain = Arc::new(AdmissionChain::from_config_parts(
            &BTreeMap::new(),
            &[with_methods(&["PUT", "DELETE"])],
        ));
        let ip = Some("10.1.2.3".parse().unwrap());
        let r = ResourceRules::new(chain, "POST", ip, true)
            .expect("a method-only deny decides per resource");
        assert!(r.refusal(Effect::Delete, "b", "any/key").is_some());
        assert!(r.refusal(Effect::Write, "b", "any/key").is_some());
        assert!(r.refusal(Effect::Read, "b", "any/key").is_none());
        assert!(r.lists_key("b", "any/key"));
    }

    #[test]
    fn a_reject_carries_its_status_and_message() {
        let r = rules(
            vec![block(
                "maint",
                Some("b"),
                Some("db/**"),
                ActionSpec::Tagged(TaggedAction::Reject {
                    status: 503,
                    message: Some("back soon".into()),
                }),
            )],
            true,
        );
        let refusal = r.refusal(Effect::Read, "b", "db/x").unwrap();
        assert_eq!(refusal.status(), 503);
        assert_eq!(refusal.message(), "back soon");
        let r = rules(vec![block("d", Some("b"), Some("x/**"), deny())], true);
        let refusal = r.refusal(Effect::Read, "b", "x/y").unwrap();
        assert_eq!(refusal.status(), 403);
        assert!(refusal.message().contains("'d'"));
    }

    /// A LIST whose every key is refused is refused outright; a prefix that
    /// only partly lies in the glob is filtered key by key instead.
    #[test]
    fn a_list_prefix_inside_the_glob_is_refused_outright() {
        let r = rules(
            vec![block("no-secret", Some("b"), Some("secret/**"), deny())],
            true,
        );
        assert!(r.list_refusal("b", "secret/").is_some());
        assert!(r.list_refusal("b", "secret/a/b").is_some());
        assert!(r.list_refusal("b", "").is_none());
        assert!(r.list_refusal("b", "sec").is_none());
        assert!(r.list_refusal("b", "team/").is_none());
        assert!(r.list_refusal("other", "secret/").is_none());
    }

    #[test]
    fn a_listing_drops_refused_keys() {
        let r = rules(
            vec![block("no-secret", Some("b"), Some("secret/**"), deny())],
            true,
        );
        assert!(!r.lists_key("b", "secret/s1.txt"));
        assert!(
            r.lists_key("b", "secret"),
            "`secret` is no key below `secret/`"
        );
        assert!(r.lists_key("b", "secrets/x"));
        assert!(r.lists_key("b", "team/x"));
    }

    /// DGP-19: with `allow-anonymous` + `path_glob: "*.zip"` an anonymous
    /// LIST shows the keys that glob matches and nothing else.
    #[test]
    fn an_anonymous_listing_shows_only_the_keys_the_glob_matches() {
        let r = rules(
            vec![block("zips", Some("pubzip"), Some("*.zip"), anon())],
            false,
        )
        .admitted_anonymously_by("zips");
        assert!(r.lists_key("pubzip", "app.zip"));
        assert!(r.lists_key("pubzip", "d/release.zip"));
        assert!(!r.lists_key("pubzip", "app.zip.bak"));
        assert!(!r.lists_key("pubzip", "release.zip.d/internal-roadmap.txt"));
        // A signed LIST is not narrowed by an anonymous carve-out.
        let r = rules(
            vec![block("zips", Some("pubzip"), Some("*.zip"), anon())],
            true,
        );
        assert!(r.lists_key("pubzip", "app.zip.bak"));
    }

    /// Only a `deny` hides a key or a folder from a listing; a key that a
    /// `reject` refuses is listed, as before the per-resource rules.
    #[test]
    fn a_reject_never_hides_a_key_or_a_folder_from_a_listing() {
        let reject = || {
            ActionSpec::Tagged(TaggedAction::Reject {
                status: 503,
                message: None,
            })
        };
        let r = rules(
            vec![block("maint", Some("b"), Some("db/**"), reject())],
            true,
        );
        assert!(r.lists_key("b", "db/a.sql"));
        assert!(r.refusal(Effect::Read, "b", "db/a.sql").is_some());
        assert_eq!(r.prefix_verdict("b", "db/"), PrefixVerdict::Shown);
        assert_eq!(r.prefix_verdict("b", ""), PrefixVerdict::Shown);
        // A LIST wholly inside the glob still answers the status.
        assert_eq!(r.list_refusal("b", "db/").unwrap().status(), 503);
        // A deny of the same glob hides.
        let r = rules(vec![block("no-db", Some("b"), Some("db/**"), deny())], true);
        assert!(!r.lists_key("b", "db/a.sql"));
        assert_eq!(r.prefix_verdict("b", "db/"), PrefixVerdict::Hidden);
        // A reject decides first: a later deny of the same key does not hide it.
        let r = rules(
            vec![
                block("maint", Some("b"), Some("db/**"), reject()),
                block("no-db", Some("b"), Some("db/**"), deny()),
            ],
            true,
        );
        assert!(r.lists_key("b", "db/a.sql"));
    }

    #[test]
    fn a_deny_beats_a_later_allow_anonymous_in_the_listing() {
        let r = rules(
            vec![
                block("no-secret", Some("b"), Some("secret/**"), deny()),
                block("zips", Some("b"), Some("**/*.zip"), anon()),
            ],
            false,
        )
        .admitted_anonymously_by("zips");
        assert!(!r.lists_key("b", "secret/a.zip"));
        assert!(r.lists_key("b", "pub/a.zip"));
    }

    #[test]
    fn the_folder_verdict_table() {
        use PrefixVerdict::*;
        let r = rules(
            vec![block("no-secret", Some("b"), Some("secret/**"), deny())],
            true,
        );
        // Every key below `secret/` is refused.
        assert_eq!(r.prefix_verdict("b", "secret/"), Hidden);
        assert_eq!(r.prefix_verdict("b", "secret/a/"), Hidden);
        // No key below `team/` can be refused.
        assert_eq!(r.prefix_verdict("b", "team/"), Shown);
        // A name that the delimiter cut inside `secret/` (`sec`): some keys
        // below it are refused, some are not.
        assert_eq!(r.prefix_verdict("b", "sec"), NeedsProof);
        assert_eq!(r.prefix_verdict("b", ""), NeedsProof);
        // Another bucket: nothing applies.
        assert_eq!(r.prefix_verdict("other", "secret/"), Shown);

        // A glob that covers part of a folder.
        let r = rules(
            vec![block("no-zip", Some("b"), Some("*.zip"), deny())],
            true,
        );
        assert_eq!(r.prefix_verdict("b", "d/"), NeedsProof);

        // An earlier carve-out in the folder turns Hidden into NeedsProof.
        let r = rules(
            vec![
                block("carve", Some("b"), Some("secret/open/*"), cont()),
                block("no-secret", Some("b"), Some("secret/**"), deny()),
            ],
            true,
        );
        assert_eq!(r.prefix_verdict("b", "secret/"), NeedsProof);
        assert_eq!(r.prefix_verdict("b", "secret/closed/"), Hidden);

        // A whole-bucket deny hides every folder; the fixed-key glob
        // `docs/readme.md` hides no folder but its own name.
        let r = rules(vec![block("no-vault", Some("b"), None, deny())], true);
        assert_eq!(r.prefix_verdict("b", "x/"), Hidden);
        let r = rules(
            vec![block("one", Some("b"), Some("docs/readme.md"), deny())],
            true,
        );
        assert_eq!(r.prefix_verdict("b", "docs/"), NeedsProof);
        assert_eq!(r.prefix_verdict("b", "img/"), Shown);
    }

    #[test]
    fn the_anonymous_folder_verdict() {
        use PrefixVerdict::*;
        let r = rules(vec![block("zips", Some("b"), Some("*.zip"), anon())], false)
            .admitted_anonymously_by("zips");
        // Any folder may hold a zip; only the keys prove it.
        assert_eq!(r.prefix_verdict("b", "a/"), NeedsProof);
        let r = rules(
            vec![block("docs", Some("b"), Some("docs/**"), anon())],
            false,
        )
        .admitted_anonymously_by("docs");
        assert_eq!(r.prefix_verdict("b", "docs/"), Shown);
        assert_eq!(r.prefix_verdict("b", "private/"), Hidden);
        assert_eq!(r.prefix_verdict("b", ""), NeedsProof);
    }

    /// The view of a bucket keeps only the blocks that can concern it (counted,
    /// never timed), lowers the name once, and answers as the one-shot methods do.
    #[test]
    fn a_bucket_view_keeps_only_the_blocks_that_can_concern_the_bucket() {
        let r = rules(
            vec![
                block("b-secret", Some("b"), Some("secret/**"), deny()),
                block("other-secret", Some("other"), Some("secret/**"), deny()),
                block("third", Some("third"), None, deny()),
                block("any-zip", None, Some("*.zip"), deny()),
            ],
            true,
        );
        let view = r.in_bucket("B");
        assert_eq!(view.block_count(), 2, "b-secret and any-zip");
        assert_eq!(r.in_bucket("nobody").block_count(), 1, "only any-zip");
        for (effect, key) in [
            (Effect::Read, "secret/x"),
            (Effect::Read, "a.zip"),
            (Effect::Write, "team/x"),
        ] {
            assert_eq!(
                view.refusal(effect, key),
                r.refusal(effect, "B", key),
                "{key}"
            );
            assert_eq!(view.lists_key(key), r.lists_key("b", key), "{key}");
        }
        assert_eq!(
            view.prefix_verdict("secret/"),
            r.prefix_verdict("b", "secret/")
        );
        assert_eq!(view.list_refusal("secret/"), r.list_refusal("b", "secret/"));
    }

    /// A folder may hold a key that a `deny` or `reject` refuses: the delta
    /// facts of its other objects are then withheld.
    #[test]
    fn a_folder_with_a_refused_key_is_found() {
        let reject = ActionSpec::Tagged(TaggedAction::Reject {
            status: 503,
            message: None,
        });
        let r = rules(
            vec![
                block("hidden", Some("b"), Some("secret/hidden*"), deny()),
                block("maint", Some("b"), Some("db/**"), reject),
                block("zips", Some("b"), Some("**/*.zip"), anon()),
            ],
            true,
        );
        let v = r.in_bucket("b");
        assert!(v.may_refuse_in_folder("secret/"));
        assert!(
            v.may_refuse_in_folder("db/x/"),
            "a reject refuses the read too"
        );
        assert!(!v.may_refuse_in_folder(""));
        assert!(
            !v.may_refuse_in_folder("team/"),
            "an allow-anonymous refuses nothing"
        );
        assert!(!r.in_bucket("other").may_refuse_in_folder("secret/"));
    }

    #[test]
    fn affects_bucket_follows_the_blocks() {
        let r = rules(vec![block("d", Some("b"), Some("x/**"), deny())], true);
        assert!(r.affects_bucket("b") && r.affects_bucket("B"));
        assert!(!r.affects_bucket("other"));
        let r = rules(vec![block("d", None, Some("x/**"), deny())], true);
        assert!(r.affects_bucket("any"));
    }
}
