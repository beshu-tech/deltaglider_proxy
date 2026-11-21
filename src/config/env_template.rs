// SPDX-License-Identifier: BUSL-1.1

//! Where an environment reference may stand (S7), and the frozen set.
//!
//! At load the proxy records the FROZEN SET: every path whose file text holds a
//! reference (with its template) and the fields that decide where a credential goes.
//! It is fixed for the life of the process; no admin request adds to it or changes it.
//!
//! A request is judged on its value tree before any expansion, then compared with the
//! running config over the frozen set. Persist and export write a template only at
//! a path that has one and escape every other string and key (`$${env:`).
//!
//! A template is stored as pieces, so `${iam:x}` written as `$${iam:x}` or as
//! `${iam:x}` is the same template: both expand to the same text.

use super::expansion::{has_unsafe_control_char, is_valid_env_name, Markers};
use super::{ConfigError, EnvRefs};
use serde_yaml::value::{Tag, TaggedValue};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One part of a template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Text that the expansion keeps (`$$` already decoded to `$`).
    Lit(String),
    /// `${env:NAME}` or `${env:NAME:-default}`.
    Ref {
        name: String,
        default: Option<String>,
    },
}

/// A string as the expander reads it: literal text and references.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Template(Vec<Piece>);

/// How [`Template::render`] and [`escape_literal`] write literal text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// Literal text as it is: for a check, never for a file.
    None,
    /// The text that an export or a section GET shows: a `$` is doubled only
    /// where the expander would read it as something else.
    Minimal,
    /// Every `$` is doubled: the persisted file.
    Full,
}

impl Template {
    /// Read `s` with the rules of the expander: `$$` is `$`, `${env:…}` is a
    /// reference, any other `${…}` stays as written, a bare `$` stays.
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        let b = s.as_bytes();
        let mut pieces = Vec::new();
        let mut lit = String::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'$' {
                let next = s[i..].find('$').map_or(s.len(), |p| i + p);
                lit.push_str(&s[i..next]);
                i = next;
                continue;
            }
            match b.get(i + 1) {
                Some(b'$') => {
                    lit.push('$');
                    i += 2;
                }
                Some(b'{') => {
                    let start = i + 2;
                    let scan = &s[start..];
                    let end = match (scan.find('}'), scan.find("${")) {
                        (Some(close), Some(open)) if open < close => {
                            return Err(ConfigError::BadEnvRef(format!(
                                "unterminated `${{` near byte {i}"
                            )))
                        }
                        (Some(close), _) => start + close,
                        (None, _) => {
                            return Err(ConfigError::BadEnvRef(format!(
                                "unterminated `${{` near byte {i}"
                            )))
                        }
                    };
                    match s[start..end].strip_prefix("env:") {
                        None => lit.push_str(&s[i..=end]),
                        Some(spec) => {
                            let (name, default) = match spec.split_once(":-") {
                                Some((n, d)) => (n, Some(d.to_string())),
                                None => (spec, None),
                            };
                            if !is_valid_env_name(name) {
                                return Err(ConfigError::BadEnvRef(format!(
                                    "invalid variable name in `${{env:{spec}}}`"
                                )));
                            }
                            if !lit.is_empty() {
                                pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                            }
                            pieces.push(Piece::Ref {
                                name: name.to_string(),
                                default,
                            });
                        }
                    }
                    i = end + 1;
                }
                _ => {
                    lit.push('$');
                    i += 1;
                }
            }
        }
        if !lit.is_empty() {
            pieces.push(Piece::Lit(lit));
        }
        Ok(Self(pieces))
    }

    /// Build from text in which each reference is a marker (see
    /// `expansion::mark_refs`) that `refs` names by index.
    pub(crate) fn from_marked(
        s: &str,
        refs: &[(String, Option<String>)],
        markers: Markers,
    ) -> Self {
        let mut pieces = Vec::new();
        let mut lit = String::new();
        let mut rest = s;
        while let Some(open) = rest.find(markers.open) {
            lit.push_str(&rest[..open]);
            let after = &rest[open + markers.open.len_utf8()..];
            let Some(close) = after.find(markers.close) else {
                break;
            };
            let index: Option<usize> = after[..close].parse().ok();
            match index.and_then(|k| refs.get(k)) {
                Some((name, default)) => {
                    if !lit.is_empty() {
                        pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                    }
                    pieces.push(Piece::Ref {
                        name: name.clone(),
                        default: default.clone(),
                    });
                }
                None => lit.push_str(
                    &rest[open..open + markers.open.len_utf8() + close + markers.close.len_utf8()],
                ),
            }
            rest = &after[close + markers.close.len_utf8()..];
        }
        lit.push_str(rest);
        if !lit.is_empty() {
            pieces.push(Piece::Lit(lit));
        }
        Self(pieces)
    }

    pub fn has_refs(&self) -> bool {
        self.0.iter().any(|p| matches!(p, Piece::Ref { .. }))
    }

    /// The template is one reference and nothing else.
    pub fn is_single_ref(&self) -> bool {
        matches!(self.0.as_slice(), [Piece::Ref { .. }])
    }

    /// The names of the references.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter_map(|p| match p {
            Piece::Ref { name, .. } => Some(name.as_str()),
            Piece::Lit(_) => None,
        })
    }

    /// Expand with `lookup`: the value of a name, else its default, else an
    /// error. A value with a control character is refused.
    pub fn expand(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<String, ConfigError> {
        let mut out = String::new();
        for piece in &self.0 {
            match piece {
                Piece::Lit(text) => out.push_str(text),
                Piece::Ref { name, default } => {
                    let value = match lookup(name) {
                        Some(v) if !v.is_empty() => v,
                        _ => match default {
                            Some(d) => d.clone(),
                            None => return Err(ConfigError::MissingEnvVar(name.clone())),
                        },
                    };
                    if has_unsafe_control_char(&value) {
                        return Err(ConfigError::UnsafeEnvValue(name.clone()));
                    }
                    out.push_str(&value);
                }
            }
        }
        Ok(out)
    }

    /// The text of the template: references as written, literal text escaped.
    pub fn render(&self, escape: Escape) -> String {
        let mut out = String::new();
        for (i, piece) in self.0.iter().enumerate() {
            match piece {
                Piece::Lit(text) => {
                    let before_ref = matches!(self.0.get(i + 1), Some(Piece::Ref { .. }));
                    out.push_str(&escape_literal(text, escape, before_ref));
                }
                Piece::Ref {
                    name,
                    default: None,
                } => out.push_str(&format!("${{env:{name}}}")),
                Piece::Ref {
                    name,
                    default: Some(d),
                } => out.push_str(&format!("${{env:{name}:-{d}}}")),
            }
        }
        out
    }
}

/// `text` written so that the expander returns it unchanged. `before_ref`:
/// a reference follows the text, so a final `$` must not join it.
pub fn escape_literal(text: &str, escape: Escape, before_ref: bool) -> String {
    match escape {
        Escape::None => text.to_string(),
        Escape::Full => text.replace('$', "$$"),
        Escape::Minimal => {
            let b = text.as_bytes();
            let mut out = String::with_capacity(text.len());
            let mut i = 0;
            while i < b.len() {
                if b[i] != b'$' {
                    let next = text[i..].find('$').map_or(text.len(), |p| i + p);
                    out.push_str(&text[i..next]);
                    i = next;
                    continue;
                }
                match b.get(i + 1) {
                    None => out.push_str(if before_ref { "$$" } else { "$" }),
                    Some(b'$') => out.push_str("$$"),
                    Some(b'{') => {
                        let scan = &text[i + 2..];
                        let closed = match (scan.find('}'), scan.find("${")) {
                            (Some(close), Some(open)) if open < close => None,
                            (Some(close), _) => Some(close),
                            (None, _) => None,
                        };
                        match closed {
                            // `${iam:x}` stays as written: it expands to itself.
                            Some(close) if !scan[..close].starts_with("env:") => {
                                out.push_str(&text[i..i + 2 + close + 1]);
                                i += 2 + close + 1;
                                continue;
                            }
                            _ => out.push_str("$$"),
                        }
                    }
                    Some(_) => out.push('$'),
                }
                i += 1;
            }
            out
        }
    }
}

// ── Paths ───────────────────────────────────────────────────────────────

/// How a document is laid out: the four sections, or flat. Paths are always
/// the flat ones (the shape of `Config` serialized), so a sectioned export,
/// a flat boot file and a storage shorthand name one field one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Sectioned,
    Flat,
}

impl Shape {
    pub fn of(doc: &Value) -> Self {
        match super::classify_shape(doc) {
            super::ConfigShape::Sectioned => Shape::Sectioned,
            _ => Shape::Flat,
        }
    }
}

#[derive(Debug, Clone)]
enum Seg {
    Key(String),
    /// The rendered label of a list item.
    Item(String),
}

/// The label of a list item: its `name` when it has one, else its index.
pub(crate) fn item_label(index: usize, item: &Value) -> String {
    match item.get("name").and_then(Value::as_str) {
        Some(name) => format!("/[{name}]"),
        None => format!("/#{index}"),
    }
}

/// The storage shorthand fields and the backend field each one sets.
fn shorthand_target(field: &str) -> Option<&'static str> {
    match field {
        "s3" => Some("endpoint"),
        "filesystem" => Some("path"),
        "region" => Some("region"),
        "access_key_id" => Some("access_key_id"),
        "secret_access_key" => Some("secret_access_key"),
        "force_path_style" => Some("force_path_style"),
        _ => None,
    }
}

/// The flat label of the node at `segs`.
fn flat_label(shape: Shape, segs: &[Seg]) -> String {
    let render = |keys: &[&str], rest: &[Seg]| -> String {
        let mut out = String::new();
        for k in keys {
            out.push('/');
            out.push_str(k);
        }
        for seg in rest {
            match seg {
                Seg::Key(k) => {
                    out.push('/');
                    out.push_str(k);
                }
                Seg::Item(label) => out.push_str(label),
            }
        }
        out
    };
    if shape == Shape::Sectioned {
        if let [Seg::Key(section), Seg::Key(field), rest @ ..] = segs {
            match (section.as_str(), field.as_str()) {
                ("admission", "blocks") => return render(&["admission_blocks"], rest),
                ("access" | "advanced", f) => return render(&[f], rest),
                ("storage", f) => {
                    return match shorthand_target(f) {
                        Some(target) => render(&["backend", target], rest),
                        None => render(&[f], rest),
                    }
                }
                _ => {}
            }
        }
    }
    render(&[], segs)
}

/// What the walk asks of its visitor.
pub trait Visitor {
    /// A string value at `label`: the visitor may replace the node.
    fn string(&mut self, label: &str, node: &mut Value) -> Result<(), ConfigError>;
    /// A map key at `label` (the path of the value it names): `Some` renames it.
    fn key(&mut self, label: &str, key: &str) -> Result<Option<String>, ConfigError>;
}

/// Walk every string value and every string map key of `doc`, with flat
/// labels. Descends into tagged nodes.
pub fn walk(doc: &mut Value, shape: Shape, visitor: &mut dyn Visitor) -> Result<(), ConfigError> {
    fn go(
        v: &mut Value,
        shape: Shape,
        segs: &mut Vec<Seg>,
        visitor: &mut dyn Visitor,
    ) -> Result<(), ConfigError> {
        match v {
            Value::String(_) => visitor.string(&flat_label(shape, segs), v),
            Value::Tagged(t) => go(&mut t.value, shape, segs, visitor),
            Value::Sequence(seq) => {
                for (i, item) in seq.iter_mut().enumerate() {
                    segs.push(Seg::Item(item_label(i, item)));
                    go(item, shape, segs, visitor)?;
                    segs.pop();
                }
                Ok(())
            }
            Value::Mapping(map) => {
                let old = std::mem::take(map);
                let mut failed = None;
                for (key, mut value) in old {
                    let key_name = key.as_str().map(str::to_string);
                    let key = match &key_name {
                        Some(k) => {
                            segs.push(Seg::Key(k.clone()));
                            let renamed = visitor.key(&flat_label(shape, segs), k);
                            match renamed {
                                Ok(Some(new)) => {
                                    segs.pop();
                                    segs.push(Seg::Key(new.clone()));
                                    Value::String(new)
                                }
                                Ok(None) => key,
                                Err(e) => {
                                    failed.get_or_insert(e);
                                    key
                                }
                            }
                        }
                        None => {
                            segs.push(Seg::Key(serde_yaml::to_string(&key).unwrap_or_default()));
                            key
                        }
                    };
                    if failed.is_none() {
                        if let Err(e) = go(&mut value, shape, segs, visitor) {
                            failed = Some(e);
                        }
                    }
                    segs.pop();
                    map.insert(key, value);
                }
                failed.map_or(Ok(()), Err)
            }
            _ => Ok(()),
        }
    }
    go(doc, shape, &mut Vec::new(), visitor)
}

// ── Recording at load ───────────────────────────────────────────────────

/// Collects the template of every string that holds a reference, and the
/// path of every map key that holds one.
#[derive(Default)]
struct Recorder {
    templates: BTreeMap<String, Template>,
    keys: Vec<String>,
}

impl Visitor for Recorder {
    fn string(&mut self, label: &str, node: &mut Value) -> Result<(), ConfigError> {
        if let Value::String(s) = node {
            if s.contains("${env:") {
                let template = Template::parse(s)?;
                if template.has_refs() {
                    self.templates.insert(label.to_string(), template);
                }
            }
        }
        Ok(())
    }

    fn key(&mut self, label: &str, key: &str) -> Result<Option<String>, ConfigError> {
        if key.contains("${env:") && Template::parse(key).is_ok_and(|t| t.has_refs()) {
            self.keys.push(label.to_string());
        }
        Ok(None)
    }
}

/// The marked value tree of config text: each reference is a marker, and the
/// strings are rewritten as expander text (`$` doubled, references written
/// out), so the tree reads like a request body.
pub fn marked_tree(text: &str) -> Result<Value, ConfigError> {
    let (marked, refs, markers) = super::expansion::mark_refs(text)?;
    let mut tree: Value =
        serde_yaml::from_str(&marked).map_err(|e| ConfigError::Parse(e.to_string()))?;
    tree.apply_merge()
        .map_err(|e| ConfigError::Parse(e.to_string()))?;
    struct Unmark<'a>(&'a [(String, Option<String>)], Markers);
    impl Visitor for Unmark<'_> {
        fn string(&mut self, _: &str, node: &mut Value) -> Result<(), ConfigError> {
            if let Value::String(s) = node {
                *s = Template::from_marked(s, self.0, self.1).render(Escape::Full);
            }
            Ok(())
        }
        fn key(&mut self, _: &str, key: &str) -> Result<Option<String>, ConfigError> {
            Ok(Some(
                Template::from_marked(key, self.0, self.1).render(Escape::Full),
            ))
        }
    }
    unmark_tags(&mut tree, markers);
    walk(&mut tree, Shape::Flat, &mut Unmark(&refs, markers))?;
    Ok(tree)
}

/// A whole-scalar marker comes tagged (`!envref`): the template tree holds
/// the plain string. An `!envref` that holds no marker came from the CLI,
/// which expanded the text itself: its tag stays, for the typed load.
fn unmark_tags(v: &mut Value, markers: Markers) {
    match v {
        Value::Tagged(t)
            if t.tag == Tag::new(super::lenient::ENV_REF_TAG)
                && t.value.as_str().is_some_and(|s| s.contains(markers.open)) =>
        {
            *v = std::mem::take(&mut t.value);
        }
        Value::Tagged(t) => unmark_tags(&mut t.value, markers),
        Value::Sequence(seq) => seq.iter_mut().for_each(|i| unmark_tags(i, markers)),
        Value::Mapping(map) => map.iter_mut().for_each(|(_, v)| unmark_tags(v, markers)),
        _ => {}
    }
}

/// The templates of a config file's text, by flat path.
pub fn record_templates(text: &str) -> Result<BTreeMap<String, Template>, ConfigError> {
    if !text.contains("${env:") {
        return Ok(BTreeMap::new());
    }
    let mut tree = marked_tree(text)?;
    let shape = Shape::of(&tree);
    let mut recorder = Recorder::default();
    walk(&mut tree, shape, &mut recorder)?;
    Ok(recorder.templates)
}

/// Record a template `${env:NAME}` at every string of the flat config tree
/// that is `value`: a secret environment value that an edit copied into
/// another field (a mode flip promotes the key to `legacy_key`). The proxy
/// does this for its own environment secrets, never for admin text.
pub fn record_secret_env_templates(tree: &Value, name: &str, value: &str, refs: &mut EnvRefs) {
    struct Find<'a>(&'a str, Vec<String>);
    impl Visitor for Find<'_> {
        fn string(&mut self, label: &str, node: &mut Value) -> Result<(), ConfigError> {
            if node.as_str() == Some(self.0) {
                self.1.push(label.to_string());
            }
            Ok(())
        }
        fn key(&mut self, _: &str, _: &str) -> Result<Option<String>, ConfigError> {
            Ok(None)
        }
    }
    let mut find = Find(value, Vec::new());
    let mut copy = tree.clone();
    let _ = walk(&mut copy, Shape::Flat, &mut find);
    if let Ok(template) = Template::parse(&format!("${{env:{name}}}")) {
        for label in find.1 {
            refs.templates.insert(label, template.clone());
        }
    }
}

// ── Judging an admin request ────────────────────────────────────────────

/// The fields that `DGP_CONFIG_ENV_ALLOWLIST` may fill: no read path returns them in
/// clear and the proxy never sends them to an address an admin sets (S3 secret, encryption keys).
pub fn is_allowlist_slot(path: &str) -> bool {
    let mut segs = path.rsplit('/');
    let leaf = segs.next().unwrap_or("");
    let parent = segs.next().unwrap_or("");
    let backend_secret = leaf == "secret_access_key"
        && (path == "/backend/secret_access_key" || path.starts_with("/backends/"));
    let encryption_key = matches!(leaf, "key" | "legacy_key")
        && (parent == "encryption" || path.starts_with("/backend_encryption/"));
    backend_secret || encryption_key
}

/// A field that a GET shows as its template, never its value: its value in a request
/// is a guess. (A typed field comes back as its value, which is no change.)
fn is_credential_field(path: &str) -> bool {
    let mut segs = path.rsplit('/');
    let leaf = segs.next().unwrap_or("");
    let parent = segs.next().unwrap_or("");
    matches!(
        leaf,
        "secret_access_key"
            | "client_secret"
            | "slack_bot_token"
            | "bootstrap_password_hash"
            | "webhook_url"
    ) || matches!(parent, "webhook_headers" | "webhook_urls")
        || is_key_material(path)
}

/// The `key` and `legacy_key` of an encryption block.
pub(super) fn is_key_material(path: &str) -> bool {
    let mut segs = path.rsplit('/');
    let leaf = segs.next().unwrap_or("");
    let parent = segs.next().unwrap_or("");
    matches!(leaf, "key" | "legacy_key")
        && (parent == "encryption" || path.starts_with("/backend_encryption/"))
}

fn refusal(label: &str, name: &str, rule: &str) -> ConfigError {
    ConfigError::EnvRefRefused(format!(
        "`${{env:{name}}}` is refused at `{label}`: {rule}. Edit the config file to move it, \
         or run `config apply`, which expands references on the operator's side."
    ))
}

const RULE_SAME_FIELD: &str = "an admin request may use an environment reference only in the \
     field where the config file has the same reference, so that a request cannot copy a \
     secret into another field";

/// The refusal of a change to a frozen field. It names the path and never a value.
pub fn frozen_refusal(label: &str) -> String {
    format!(
        "`{label}`: this field comes from the environment: change it in the config file and \
         restart"
    )
}

/// Judge and expand the strings of an admin request. Pure over its inputs.
pub struct Admit<'a> {
    pub running: &'a EnvRefs,
    /// Is this name allowlisted?
    pub allowed: &'a dyn Fn(&str) -> bool,
    /// The server's environment, read only for an allowlisted name.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// A whole-scalar reference becomes an `!envref` node (a YAML document:
    /// the typed load gives it the type of its field).
    pub tag_whole: bool,
    /// The references that the request added through the allowlist.
    pub added: EnvRefs,
}

impl<'a> Admit<'a> {
    pub fn new(
        running: &'a EnvRefs,
        allowed: &'a dyn Fn(&str) -> bool,
        env: &'a dyn Fn(&str) -> Option<String>,
        tag_whole: bool,
    ) -> Self {
        Self {
            running,
            allowed,
            env,
            tag_whole,
            added: EnvRefs::default(),
        }
    }
}

impl Visitor for Admit<'_> {
    fn string(&mut self, label: &str, node: &mut Value) -> Result<(), ConfigError> {
        let Value::String(s) = node else {
            return Ok(());
        };
        let frozen = self.running.frozen.templates.get(label);
        if frozen.is_none() && !s.contains('$') {
            return Ok(());
        }
        let template = Template::parse(s)
            .map_err(|e| ConfigError::EnvRefRefused(format!("`{label}`: {e}")))?;
        // A frozen field keeps its template: a literal and the right guess are refused
        // alike. A typed field that a GET showed as its value may come back as it.
        if let Some(f) = frozen.filter(|f| **f != template) {
            let same_value = !template.has_refs()
                && !is_credential_field(label)
                && template.expand(|_| None).ok()
                    == f.expand(|n| self.running.get(n).cloned()).ok();
            if !same_value {
                return Err(ConfigError::EnvRefRefused(frozen_refusal(label)));
            }
            *node = Value::String(template.expand(|_| None).unwrap_or_default());
            return Ok(());
        }
        // A template that this process added itself (a restore, an allowlisted slot)
        // is known too; it stands only in a field that nothing reads back.
        let recorded = frozen.is_some() || self.running.templates.get(label) == Some(&template);
        let allowlisted = !recorded
            && template.is_single_ref()
            && template.names().all(|n| (self.allowed)(n))
            && is_allowlist_slot(label);
        if template.has_refs() && !recorded && !allowlisted {
            let name = template.names().next().unwrap_or_default();
            return Err(refusal(label, name, RULE_SAME_FIELD));
        }
        let value = template
            .expand(|name| {
                if allowlisted {
                    (self.env)(name)
                } else {
                    self.running.get(name).cloned()
                }
            })
            .map_err(|e| ConfigError::EnvRefRefused(format!("`{label}`: {e}")))?;
        if allowlisted {
            for name in template.names() {
                if let Some(v) = (self.env)(name) {
                    self.added.insert(name.to_string(), v);
                }
            }
            self.added
                .templates
                .insert(label.to_string(), template.clone());
        }
        *node = if self.tag_whole && template.is_single_ref() {
            Value::Tagged(Box::new(TaggedValue {
                tag: Tag::new(super::lenient::ENV_REF_TAG),
                value: Value::String(value),
            }))
        } else {
            Value::String(value)
        };
        Ok(())
    }

    fn key(&mut self, label: &str, key: &str) -> Result<Option<String>, ConfigError> {
        if !key.contains('$') {
            return Ok(None);
        }
        let template = Template::parse(key)
            .map_err(|e| ConfigError::EnvRefRefused(format!("`{label}`: {e}")))?;
        if let Some(name) = template.names().next() {
            return Err(refusal(
                label,
                name,
                "a map key cannot hold an environment reference",
            ));
        }
        template.expand(|_| None).map(Some)
    }
}

/// Judge and expand a YAML document that an admin sent (apply, validate,
/// import, a backup's config). The returned tree is document-shaped and
/// holds no reference: feed it to the typed load. `added` is what the
/// allowlist brought in.
pub fn admit_document(
    text: &str,
    running: &EnvRefs,
    allowed: &dyn Fn(&str) -> bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(Value, EnvRefs), ConfigError> {
    let mut tree = marked_tree(text)?;
    let shape = Shape::of(&tree);
    let mut admit = Admit::new(running, allowed, env, true);
    walk(&mut tree, shape, &mut admit)?;
    Ok((tree, admit.added))
}

/// [`admit_document`] for the body of a section PUT (JSON, one section).
pub fn admit_section_body(
    section: &str,
    body: &serde_json::Value,
    running: &EnvRefs,
    allowed: &dyn Fn(&str) -> bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(serde_json::Value, EnvRefs), ConfigError> {
    let inner = serde_yaml::to_value(body).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let mut map = serde_yaml::Mapping::new();
    map.insert(Value::String(section.to_string()), inner);
    let mut doc = Value::Mapping(map);
    let mut admit = Admit::new(running, allowed, env, false);
    walk(&mut doc, Shape::Sectioned, &mut admit)?;
    let out = doc
        .get(section)
        .cloned()
        .ok_or_else(|| ConfigError::Parse("internal: the section vanished".to_string()))?;
    let out = serde_json::to_value(out).map_err(|e| ConfigError::Parse(e.to_string()))?;
    Ok((out, admit.added))
}

// ── Writing the file text ───────────────────────────────────────────────

/// The text that persist or export writes for the string `s` at `label`: the
/// template where there is one and `s` is still what it expands to, else `s`
/// escaped.
pub fn file_text(label: &str, s: &str, refs: &EnvRefs, escape: Escape) -> String {
    if let Some(template) = refs.template_at(label) {
        if template.expand(|n| refs.get(n).cloned()).ok().as_deref() == Some(s) {
            return template.render(escape);
        }
    }
    escape_literal(s, escape, false)
}

/// The text of a scalar node, as a template expansion would give it.
fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Call `f` on every scalar of `doc` with its flat label.
pub(super) fn visit_scalars(doc: &mut Value, shape: Shape, f: &mut dyn FnMut(&str, &mut Value)) {
    fn go(v: &mut Value, shape: Shape, segs: &mut Vec<Seg>, f: &mut dyn FnMut(&str, &mut Value)) {
        match v {
            Value::Tagged(t) => go(&mut t.value, shape, segs, f),
            Value::Sequence(seq) => {
                for (i, item) in seq.iter_mut().enumerate() {
                    segs.push(Seg::Item(item_label(i, item)));
                    go(item, shape, segs, f);
                    segs.pop();
                }
            }
            Value::Mapping(map) => {
                for (key, value) in map.iter_mut() {
                    let Some(k) = key.as_str() else { continue };
                    segs.push(Seg::Key(k.to_string()));
                    go(value, shape, segs, f);
                    segs.pop();
                }
            }
            Value::String(_) | Value::Number(_) | Value::Bool(_) => f(&flat_label(shape, segs), v),
            Value::Null => {}
        }
    }
    go(doc, shape, &mut Vec::new(), f)
}

/// The text with a template written at every typed field (number, flag, enum) whose value
/// is still what it expands to: the typed `Config` cannot hold the template itself.
pub fn restore_typed_templates(
    text: &str,
    refs: &EnvRefs,
    escape: Escape,
) -> Result<String, ConfigError> {
    if refs.frozen.templates.is_empty() && refs.templates.is_empty() {
        return Ok(text.to_string());
    }
    let mut tree: Value =
        serde_yaml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let shape = Shape::of(&tree);
    let mut changed = false;
    visit_scalars(&mut tree, shape, &mut |label, node| {
        let Some(template) = refs.template_at(label) else {
            return;
        };
        let rendered = template.render(escape);
        if matches!(node, Value::String(s) if *s == rendered) {
            return;
        }
        let expanded = template.expand(|n| refs.get(n).cloned()).ok();
        if expanded.is_some() && expanded == scalar_text(node) {
            *node = Value::String(rendered);
            changed = true;
        }
    });
    if !changed {
        return Ok(text.to_string());
    }
    serde_yaml::to_string(&tree).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// The last gate before a write: a restart reads this text, so it may hold a reference
/// only at a path whose template `refs` has, never in a key. Fails closed.
pub fn refuse_unrecorded_refs(text: &str, refs: &EnvRefs) -> Result<(), ConfigError> {
    if !text.contains("${env:") {
        return Ok(());
    }
    let mut tree = marked_tree(text)?;
    let shape = Shape::of(&tree);
    let mut scan = Recorder::default();
    walk(&mut tree, shape, &mut scan)?;
    let unrecorded = |what: String| {
        ConfigError::EnvRefRefused(format!(
            "refusing to write the config: `{what}` would hold an environment reference that \
             the config file does not have there"
        ))
    };
    if let Some(label) = scan.keys.into_iter().next() {
        return Err(unrecorded(label));
    }
    for (label, template) in scan.templates {
        if refs.template_at(&label) != Some(&template) {
            return Err(unrecorded(label));
        }
    }
    Ok(())
}

/// The document `config apply` sends: references expanded against the OPERATOR's env and
/// escaped, except those the server's export holds at the same path; a whole one is `!envref`.
pub fn cli_document(
    operator_text: &str,
    server_export: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    let server = record_templates(server_export)?;
    let kept: BTreeSet<String> = record_templates(operator_text)?
        .into_iter()
        .filter(|(label, template)| server.get(label) == Some(template))
        .map(|(label, _)| label)
        .collect();
    struct Expand<'a> {
        kept: &'a BTreeSet<String>,
        env: &'a dyn Fn(&str) -> Option<String>,
    }
    impl Visitor for Expand<'_> {
        fn string(&mut self, label: &str, node: &mut Value) -> Result<(), ConfigError> {
            let Value::String(s) = node else {
                return Ok(());
            };
            if !s.contains('$') || self.kept.contains(label) {
                return Ok(());
            }
            let template = Template::parse(s)?;
            let text = escape_literal(&template.expand(self.env)?, Escape::Full, false);
            *node = if template.is_single_ref() {
                Value::Tagged(Box::new(TaggedValue {
                    tag: Tag::new(super::lenient::ENV_REF_TAG),
                    value: Value::String(text),
                }))
            } else {
                Value::String(text)
            };
            Ok(())
        }
        fn key(&mut self, _: &str, key: &str) -> Result<Option<String>, ConfigError> {
            if !key.contains('$') {
                return Ok(None);
            }
            let template = Template::parse(key)?;
            Ok(Some(escape_literal(
                &template.expand(self.env)?,
                Escape::Full,
                false,
            )))
        }
    }
    let mut tree = marked_tree(operator_text)?;
    let shape = Shape::of(&tree);
    walk(&mut tree, shape, &mut Expand { kept: &kept, env })?;
    serde_yaml::to_string(&tree).map_err(|e| ConfigError::Parse(e.to_string()))
}

// ── The frozen set ──────────────────────────────────────────────────────

/// The paths that the config file gave to the environment, recorded when the
/// file is loaded and never changed afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frozen {
    /// `path → template` of each string of the file that holds a reference.
    pub templates: BTreeMap<String, Template>,
    /// The fields of an object that decide where its frozen credentials go
    /// (see [`CREDENTIAL_DESTINATIONS`]).
    pub destinations: BTreeSet<String>,
}

impl Frozen {
    /// The frozen set of the file `text`. `flat_tree` is the loaded config
    /// serialized (the shape of `Config`): it names the objects.
    pub fn record(text: &str, flat_tree: &Value) -> Result<Self, ConfigError> {
        let templates = record_templates(text)?;
        let mut destinations = BTreeSet::new();
        for site in CREDENTIAL_DESTINATIONS {
            for (object, node) in select(flat_tree, "", site.object) {
                let holds_frozen = site.credentials.iter().any(|pattern| {
                    select(node, &object, pattern)
                        .iter()
                        .any(|(label, _)| templates.contains_key(label))
                });
                if holds_frozen {
                    for destination in site.destinations {
                        destinations.insert(format!("{object}{destination}"));
                    }
                }
            }
        }
        Ok(Self {
            templates,
            destinations,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Does the file give the `field` of the item `name` of `list` (for
    /// example `auth_providers`, `corp`, `client_secret`) to the environment?
    pub fn holds_item(&self, list: &str, name: &str, field: &str) -> bool {
        self.templates
            .contains_key(&format!("/{list}/[{name}]/{field}"))
    }
}

/// One object of the config that holds credentials the proxy SENDS in clear
/// to a destination that sibling fields name.
pub struct CredentialSite {
    /// The path of the object; `*` is one list item or one map entry.
    pub object: &'static str,
    /// Credential fields of the object (`*` as above).
    pub credentials: &'static [&'static str],
    /// Fields of the same object that decide where they go.
    pub destinations: &'static [&'static str],
}

/// THE table. An S3 backend's access key and secret are not here: SigV4
/// signs the request, and the secret never leaves the host (its session
/// token is never read from the config, `#[serde(skip)]`). The Slack bot
/// token goes to the one fixed Slack API address (`event_delivery.rs`), so
/// it has no destination field. A Slack incoming-webhook URL is its own
/// credential and its own destination: it is frozen like any field that
/// holds a reference.
pub const CREDENTIAL_DESTINATIONS: &[CredentialSite] = &[
    CredentialSite {
        object: "/event_delivery",
        credentials: &["/webhook_headers/*"],
        destinations: &["/webhook_url", "/webhook_urls", "/allow_local"],
    },
    CredentialSite {
        object: "/auth_providers/*",
        credentials: &["/client_secret"],
        destinations: &["/issuer_url", "/provider_type", "/extra_config"],
    },
];

/// The nodes that `pattern` (`/a/*/b`) names under `node`, with their labels.
fn select<'a>(node: &'a Value, label: &str, pattern: &str) -> Vec<(String, &'a Value)> {
    let Some(rest) = pattern.strip_prefix('/') else {
        return vec![(label.to_string(), node)];
    };
    let (head, tail) = match rest.split_once('/') {
        Some((h, t)) => (h, Some(format!("/{t}"))),
        None => (rest, None),
    };
    let mut found: Vec<(String, &Value)> = Vec::new();
    if head == "*" {
        match node {
            Value::Sequence(seq) => {
                for (i, item) in seq.iter().enumerate() {
                    found.push((format!("{label}{}", item_label(i, item)), item));
                }
            }
            Value::Mapping(map) => {
                for (k, v) in map {
                    if let Some(k) = k.as_str() {
                        found.push((format!("{label}/{k}"), v));
                    }
                }
            }
            _ => {}
        }
    } else if let Some(child) = node.get(head) {
        found.push((format!("{label}/{head}"), child));
    }
    match tail {
        None => found,
        Some(tail) => found
            .into_iter()
            .flat_map(|(l, n)| select(n, &l, &tail))
            .collect(),
    }
}

/// Every node of a flat config tree by its label.
pub(super) fn index_nodes<'a>(
    node: &'a Value,
    label: &mut String,
    out: &mut BTreeMap<String, &'a Value>,
) {
    out.insert(label.clone(), node);
    let len = label.len();
    match node {
        Value::Mapping(map) => {
            for (k, v) in map {
                if let Some(k) = k.as_str() {
                    label.push('/');
                    label.push_str(k);
                    index_nodes(v, label, out);
                    label.truncate(len);
                }
            }
        }
        Value::Sequence(seq) => {
            for (i, item) in seq.iter().enumerate() {
                label.push_str(&item_label(i, item));
                index_nodes(item, label, out);
                label.truncate(len);
            }
        }
        _ => {}
    }
}

/// A `__dgmigrate_*` route belongs to the migrate job, not to any request.
pub(crate) fn is_server_route(label: &str) -> bool {
    label
        .strip_prefix("/buckets/")
        .is_some_and(|rest| rest.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX))
}

/// The IAM state that lives in the config database when the mode is `gui`.
const IAM_BACKED: [&str; 4] = [
    "/iam_users/",
    "/iam_groups/",
    "/auth_providers/",
    "/group_mapping_rules/",
];

/// `Some(message)` when `new` differs from `old` at a frozen path or destination, or
/// leaves `declarative` while a frozen path is in the IAM state. Names the path, no value.
/// Only the migrate job's own writer (`server_writer`) may change a `__dgmigrate_*` route.
pub fn frozen_violation(
    refs: &EnvRefs,
    old: &Value,
    new: &Value,
    server_writer: bool,
) -> Option<String> {
    let frozen = &*refs.frozen;
    if frozen.is_empty() {
        return None;
    }
    let (mut before, mut after) = (BTreeMap::new(), BTreeMap::new());
    index_nodes(old, &mut String::new(), &mut before);
    index_nodes(new, &mut String::new(), &mut after);
    for label in frozen
        .templates
        .keys()
        .filter(|l| !(server_writer && is_server_route(l)))
    {
        if before.get(label) != after.get(label) {
            return Some(frozen_refusal(label));
        }
    }
    for label in &frozen.destinations {
        if before.get(label.as_str()) != after.get(label.as_str()) {
            return Some(format!(
                "`{label}`: this field belongs to an object whose credential comes from the \
                 environment: change it in the config file and restart"
            ));
        }
    }
    let mode = |nodes: &BTreeMap<String, &Value>| {
        nodes
            .get("/iam_mode")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let declarative = |m: Option<String>| m.as_deref() == Some("declarative");
    if declarative(mode(&before)) && !declarative(mode(&after)) {
        if let Some(label) = frozen
            .templates
            .keys()
            .find(|l| IAM_BACKED.iter().any(|p| l.starts_with(p)))
        {
            return Some(format!(
                "`/iam_mode`: the mode cannot leave `declarative` while `{label}` comes from the \
                 environment: change it in the config file and restart"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests;
