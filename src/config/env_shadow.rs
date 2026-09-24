// SPDX-License-Identifier: BUSL-1.1

//! What the config FILE says for every field an environment variable
//! overrides.
//!
//! `DGP_*` variables win over the file at load (`apply_env_overrides`). The
//! running config therefore holds env values, and a persist or an export of
//! it would copy those values — secrets included — into the YAML file. The
//! [`EnvShadow`] records, for each override "slot" (a path in the serialized
//! flat `Config`), the value the file had before the override. Persist and
//! export write the file view ([`restore`]); the runtime keeps the env value.
//!
//! Pure functions over `serde_yaml::Value` trees; unit-tested below.

use serde_yaml::Value;
use std::collections::BTreeMap;

/// One step of a slot path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlotSeg {
    /// A mapping key.
    Key(String),
    /// The element of a sequence whose `name` field equals this string
    /// (named backends: order-independent, survives reordering).
    Named(String),
}

/// A path to one env-controlled node of the serialized flat `Config`.
pub type EnvSlot = Vec<SlotSeg>;

/// Build a slot from mapping keys.
pub fn slot(keys: &[&str]) -> EnvSlot {
    keys.iter()
        .map(|k| SlotSeg::Key((*k).to_string()))
        .collect()
}

/// Slot → the value the file had there (`None` = the key was absent).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvShadow(pub BTreeMap<EnvSlot, Option<Value>>);

impl EnvShadow {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The node at `path`, or `None` when any step is missing.
pub fn get<'a>(tree: &'a Value, path: &[SlotSeg]) -> Option<&'a Value> {
    let mut node = tree;
    for seg in path {
        node = match (seg, node) {
            (SlotSeg::Key(k), Value::Mapping(m)) => m.get(Value::String(k.clone()))?,
            (SlotSeg::Named(n), Value::Sequence(s)) => s
                .iter()
                .find(|el| el.get("name").and_then(Value::as_str) == Some(n.as_str()))?,
            _ => return None,
        };
    }
    Some(node)
}

fn get_mut<'a>(tree: &'a mut Value, path: &[SlotSeg]) -> Option<&'a mut Value> {
    let mut node = tree;
    for seg in path {
        node = match (seg, node) {
            (SlotSeg::Key(k), Value::Mapping(m)) => m.get_mut(Value::String(k.clone()))?,
            (SlotSeg::Named(n), Value::Sequence(s)) => s
                .iter_mut()
                .find(|el| el.get("name").and_then(Value::as_str) == Some(n.as_str()))?,
            _ => return None,
        };
    }
    Some(node)
}

/// Set (`Some`) or remove (`None`) the node at `path`. A missing parent is
/// left alone: the slot no longer exists (a removed named backend).
pub fn put(tree: &mut Value, path: &[SlotSeg], value: Option<Value>) {
    let Some((SlotSeg::Key(last), parent_path)) = path.split_last() else {
        return;
    };
    let Some(Value::Mapping(parent)) = get_mut(tree, parent_path) else {
        return;
    };
    let key = Value::String(last.clone());
    match value {
        Some(v) => {
            parent.insert(key, v);
        }
        None => {
            parent.remove(&key);
        }
    }
}

/// Record what `before` (the file view) holds at each applied slot.
pub fn capture(before: &Value, applied: &[EnvSlot]) -> EnvShadow {
    EnvShadow(
        applied
            .iter()
            .map(|s| (s.clone(), get(before, s).cloned()))
            .collect(),
    )
}

/// Write the file view: every shadowed slot gets the file's value back.
pub fn restore(tree: &mut Value, shadow: &EnvShadow) {
    for (path, value) in &shadow.0 {
        put(tree, path, value.clone());
    }
}

/// Turn an incoming config tree (an edit built from the running config) into
/// its file view. A slot whose incoming value still equals the running (env)
/// value is an echo of the env value, so it takes the file's value from the
/// running shadow. A slot the edit changed keeps the edit: that is what the
/// operator authored for the file (the env still wins at runtime).
///
/// Returns the slots the edit really changed (the incoming value differs from
/// both the env value and the file value), so the caller can tell the
/// operator that the edit reaches the file but not the runtime.
pub fn unapply_echoes(
    incoming: &mut Value,
    running: &Value,
    running_shadow: &EnvShadow,
) -> Vec<EnvSlot> {
    let mut edited = Vec::new();
    for (path, file_value) in &running_shadow.0 {
        let now = get(incoming, path);
        if now == get(running, path) {
            put(incoming, path, file_value.clone());
        } else if now != file_value.as_ref() {
            edited.push(path.clone());
        }
    }
    edited
}

/// Dotted, human-readable form of a slot (`backends[eu].encryption.key`).
pub fn display(path: &[SlotSeg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            SlotSeg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            SlotSeg::Named(n) => {
                out.push('[');
                out.push_str(n);
                out.push(']');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn y(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn get_and_put_by_key_and_by_name() {
        let mut t = y("a: {b: 1}\nbackends: [{name: x, enc: {key: k1}}, {name: z}]");
        assert_eq!(get(&t, &slot(&["a", "b"])), Some(&y("1")));
        let named = vec![
            SlotSeg::Key("backends".into()),
            SlotSeg::Named("x".into()),
            SlotSeg::Key("enc".into()),
            SlotSeg::Key("key".into()),
        ];
        assert_eq!(get(&t, &named), Some(&y("k1")));
        put(&mut t, &named, Some(y("k2")));
        assert_eq!(get(&t, &named), Some(&y("k2")));
        put(&mut t, &named, None);
        assert_eq!(get(&t, &named), None);
        // Missing parent: nothing happens.
        let gone = vec![
            SlotSeg::Key("backends".into()),
            SlotSeg::Named("nope".into()),
            SlotSeg::Key("key".into()),
        ];
        let before = t.clone();
        put(&mut t, &gone, Some(y("v")));
        assert_eq!(t, before);
    }

    #[test]
    fn capture_then_restore_gives_back_the_file_view() {
        let file = y("secret: file-s\ncache: 10\ntls: null\nkeep: 1");
        let slots = vec![slot(&["secret"]), slot(&["cache"]), slot(&["tls"])];
        let shadow = capture(&file, &slots);
        let mut running = y("secret: env-s\ncache: 99\ntls: {enabled: true}\nkeep: 2");
        restore(&mut running, &shadow);
        assert_eq!(running, y("secret: file-s\ncache: 10\ntls: null\nkeep: 2"));
    }

    #[test]
    fn absent_file_value_is_removed_on_restore() {
        let shadow = capture(&y("a: 1"), &[slot(&["secret"])]);
        let mut running = y("a: 1\nsecret: env-s");
        restore(&mut running, &shadow);
        assert_eq!(running, y("a: 1"));
    }

    #[test]
    fn echoes_take_the_file_value_and_edits_are_kept() {
        let running = y("secret: env-s\ncache: 99");
        let shadow = capture(
            &y("secret: file-s\ncache: 10"),
            &[slot(&["secret"]), slot(&["cache"])],
        );
        // The secret is echoed back unchanged; the cache size was edited.
        let mut incoming = y("secret: env-s\ncache: 50");
        let edited = unapply_echoes(&mut incoming, &running, &shadow);
        assert_eq!(incoming, y("secret: file-s\ncache: 50"));
        assert_eq!(edited, vec![slot(&["cache"])]);
        // Echoing the FILE value (an import of an export) is not an edit.
        let mut incoming = y("secret: file-s\ncache: 10");
        assert!(unapply_echoes(&mut incoming, &running, &shadow).is_empty());
        assert_eq!(
            display(&[
                SlotSeg::Key("backends".into()),
                SlotSeg::Named("eu".into()),
                SlotSeg::Key("key".into())
            ]),
            "backends[eu].key"
        );
    }
}
