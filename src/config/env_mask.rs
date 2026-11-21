// SPDX-License-Identifier: BUSL-1.1

//! The guard against a server-side copy of a field that comes from a reference: a writer
//! edits a masked view, and a mask that lands where no copy was declared refuses the write.

use super::env_template::{index_nodes, is_key_material, is_server_route, visit_scalars, Shape};
use super::{Config, EnvRefs};
use serde_yaml::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One random mask per masked path; a key gets 64 hex digits, so it parses as a key.
#[derive(Debug, Clone, Default)]
struct Masks(BTreeMap<String, String>);

impl Masks {
    fn make(label: &str) -> String {
        let id = || uuid::Uuid::new_v4().simple().to_string();
        if is_key_material(label) {
            format!("{}{}", id(), id())
        } else {
            format!("frozen-{}", id())
        }
    }

    fn owner_of(&self) -> BTreeMap<&str, &str> {
        self.0
            .iter()
            .map(|(label, mask)| (mask.as_str(), label.as_str()))
            .collect()
    }
}

/// Why [`MaskedView::finish`] refused.
#[derive(Debug, PartialEq, Eq)]
pub enum MaskRefusal {
    /// A frozen path lost its mask: the writer changed it.
    Changed(String),
    /// A mask stands where it is not its own and no copy was declared.
    Copied(String),
    Internal(String),
}

impl MaskRefusal {
    pub fn message(&self) -> String {
        match self {
            Self::Changed(label) => super::env_template::frozen_refusal(label),
            Self::Copied(label) => format!(
                "`{label}`: the server tried to copy a field that comes from the environment to \
                 a field that has no reference for it. This is a fault of the proxy, not of the \
                 request: nothing was changed"
            ),
            Self::Internal(e) => format!("internal: {e}"),
        }
    }
}

/// The running config with a mask in every string that a reference supplies.
pub struct MaskedView {
    /// What the writer edits.
    pub config: Config,
    before: Value,
    masks: Masks,
    old_refs: EnvRefs,
    server_writer: bool,
}

/// The `#[serde(skip)]` fields, which a round trip through the tree loses.
fn carry_runtime(from: &Config, to: &mut Config) {
    to.env_refs = from.env_refs.clone();
    to.env_shadow = from.env_shadow.clone();
    to.tuning = from.tuning.clone();
}

fn paths_of(refs: &EnvRefs) -> BTreeSet<String> {
    refs.frozen
        .templates
        .keys()
        .chain(refs.templates.keys())
        .cloned()
        .collect()
}

impl MaskedView {
    /// Mask `old`. `server_writer`: the migrate job's writer, which owns `__dgmigrate_*` routes.
    pub fn of(old: &Config, server_writer: bool) -> Result<Self, MaskRefusal> {
        let internal = |e: serde_yaml::Error| MaskRefusal::Internal(e.to_string());
        let before = serde_yaml::to_value(old).map_err(internal)?;
        let holders = paths_of(&old.env_refs);
        let mut masks = Masks::default();
        let mut tree = before.clone();
        for label in &holders {
            masks.0.insert(label.clone(), Masks::make(label));
        }
        let put = |tree: &mut Value, only: Option<&str>, masks: &Masks| {
            let mut hit = Vec::new();
            visit_scalars(tree, Shape::Flat, &mut |at, node| {
                if only.is_some_and(|l| l != at) {
                    return;
                }
                if let (Value::String(_), Some(mask)) = (&*node, masks.0.get(at)) {
                    *node = Value::String(mask.clone());
                    hit.push(at.to_string());
                }
            });
            hit
        };
        let fits = |t: &Value| serde_yaml::from_value::<Config>(t.clone()).is_ok();
        let mut done = put(&mut tree, None, &masks);
        if !fits(&tree) {
            // A typed field cannot hold the text: mask the paths one at a time.
            tree = before.clone();
            done.clear();
            for label in &holders {
                let mut attempt = tree.clone();
                if !put(&mut attempt, Some(label), &masks).is_empty() && fits(&attempt) {
                    tree = attempt;
                    done.push(label.clone());
                }
            }
        }
        masks.0.retain(|label, _| done.contains(label));
        let mut config: Config = serde_yaml::from_value(tree).map_err(internal)?;
        carry_runtime(old, &mut config);
        Ok(Self {
            config,
            before,
            masks,
            old_refs: old.env_refs.clone(),
            server_writer,
        })
    }

    /// The writer's config with every mask back at its value: a mask with no declared
    /// copy is `Copied`, a frozen path that lost its mask is `Changed`.
    pub fn finish(self, made: Config) -> Result<Config, MaskRefusal> {
        if self.masks.0.is_empty() {
            return Ok(made);
        }
        let internal = |e: serde_yaml::Error| MaskRefusal::Internal(e.to_string());
        let mut tree = serde_yaml::to_value(&made).map_err(internal)?;
        let mut old_nodes = BTreeMap::new();
        index_nodes(&self.before, &mut String::new(), &mut old_nodes);
        let owner = self.masks.owner_of();
        let mut kept = BTreeSet::new();
        visit_scalars(&mut tree, Shape::Flat, &mut |at, node| {
            let Some(label) = node.as_str().and_then(|s| owner.get(s)).copied() else {
                return;
            };
            let declared = label != at
                && made.env_refs.template_at(at).is_some()
                && made.env_refs.template_at(at) == self.old_refs.template_at(label);
            if label == at || declared {
                if let Some(value) = old_nodes.get(label) {
                    *node = (*value).clone();
                    if label == at {
                        kept.insert(at.to_string());
                    }
                }
            }
        });
        if let Some(label) = find_mask(&tree, &self.masks) {
            tracing::error!(
                "a config writer copied a field that comes from the environment to `{label}` \
                 with no declared copy; the write is refused (a bug of the proxy)"
            );
            return Err(MaskRefusal::Copied(label));
        }
        let moved = self.masks.0.keys().find(|label| {
            self.old_refs.frozen.templates.contains_key(*label)
                && !kept.contains(*label)
                && !(self.server_writer && is_server_route(label))
        });
        if let Some(label) = moved {
            return Err(MaskRefusal::Changed(label.clone()));
        }
        let mut config: Config = serde_yaml::from_value(tree).map_err(internal)?;
        carry_runtime(&made, &mut config);
        Ok(config)
    }
}

/// The path of the first string (a value, a part of one or a map key) that holds a mask.
fn find_mask(tree: &Value, masks: &Masks) -> Option<String> {
    let holds = |s: &str| masks.0.values().any(|m| s.contains(m.as_str()));
    fn go(v: &Value, label: &mut String, holds: &dyn Fn(&str) -> bool) -> Option<String> {
        match v {
            Value::String(s) => holds(s).then(|| label.clone()),
            Value::Tagged(t) => go(&t.value, label, holds),
            Value::Sequence(seq) => {
                for (i, item) in seq.iter().enumerate() {
                    let len = label.len();
                    label.push_str(&super::env_template::item_label(i, item));
                    let found = go(item, label, holds);
                    label.truncate(len);
                    if found.is_some() {
                        return found;
                    }
                }
                None
            }
            Value::Mapping(map) => {
                for (k, v) in map {
                    let len = label.len();
                    label.push('/');
                    let key = k.as_str().unwrap_or_default();
                    label.push_str(key);
                    let found = if holds(key) {
                        Some(label.clone())
                    } else {
                        go(v, label, holds)
                    };
                    label.truncate(len);
                    if found.is_some() {
                        return found;
                    }
                }
                None
            }
            _ => None,
        }
    }
    go(tree, &mut String::new(), &holds)
}
