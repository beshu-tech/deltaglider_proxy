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
///
/// `file_secrets` holds the secret env values that the config file itself
/// contained when it was loaded (an operator may put the same value in the
/// file and in the environment). Writing those back is not a leak.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvShadow(
    pub BTreeMap<EnvSlot, Option<Value>>,
    pub std::collections::BTreeSet<String>,
);

impl EnvShadow {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every string the file held in a shadowed slot (at any depth).
    pub fn file_strings(&self) -> Vec<String> {
        fn walk(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::String(s) => out.push(s.clone()),
                Value::Sequence(seq) => seq.iter().for_each(|i| walk(i, out)),
                Value::Mapping(m) => m.values().for_each(|i| walk(i, out)),
                _ => {}
            }
        }
        let mut out: Vec<String> = self.1.iter().cloned().collect();
        for v in self.0.values().flatten() {
            walk(v, &mut out);
        }
        out
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
        Default::default(),
    )
}

/// Write the file view: every shadowed slot gets the file's value back.
pub fn restore(tree: &mut Value, shadow: &EnvShadow) {
    for (path, value) in &shadow.0 {
        put(tree, path, value.clone());
    }
}

/// What [`unapply_echoes`] found, as slot-relative leaf paths.
#[derive(Debug, Default, PartialEq)]
pub struct Unapplied {
    /// Leaves the edit really changed (the incoming value differs from both
    /// the env value and the file value): saved to the file, not in effect.
    pub edited: Vec<EnvSlot>,
    /// Leaves whose incoming value equals the env value while the file holds
    /// a different value: treated as an echo, so the file keeps its value.
    pub echoed_over_file: Vec<EnvSlot>,
}

/// Turn an incoming config tree (an edit built from the running config) into
/// its file view.
///
/// Composite slots (`backend`, `tls`) are compared MEMBER BY MEMBER: a member
/// whose incoming value still equals the running (env) value is an echo and
/// takes the file's value back; a member the edit changed keeps the edit.
/// Comparing a composite slot as a whole would write every echoed env member
/// — an S3 secret among them — into the file as soon as one member changed.
pub fn unapply_echoes(
    incoming: &mut Value,
    running: &Value,
    running_shadow: &EnvShadow,
) -> Unapplied {
    let mut out = Unapplied::default();
    for (path, file_value) in &running_shadow.0 {
        let now = get(incoming, path).cloned();
        let merged = merge_node(
            now,
            get(running, path),
            file_value.as_ref(),
            &mut path.clone(),
            &mut out,
        );
        put(incoming, path, merged);
    }
    out
}

/// Member-wise file view of one node. See [`unapply_echoes`].
fn merge_node(
    incoming: Option<Value>,
    running: Option<&Value>,
    file: Option<&Value>,
    path: &mut EnvSlot,
    out: &mut Unapplied,
) -> Option<Value> {
    if incoming.as_ref() == running {
        if incoming.as_ref() != file && is_leaf(incoming.as_ref()) {
            out.echoed_over_file.push(path.clone());
        }
        return file.cloned();
    }
    match (incoming, running) {
        (Some(Value::Mapping(inc)), Some(Value::Mapping(run))) => {
            let file_map = file.and_then(Value::as_mapping);
            let mut keys: Vec<Value> = Vec::new();
            for k in inc
                .keys()
                .chain(run.keys())
                .chain(file_map.into_iter().flat_map(|m| m.keys()))
            {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            let mut merged = serde_yaml::Mapping::new();
            for k in keys {
                path.push(SlotSeg::Key(k.as_str().unwrap_or_default().to_string()));
                let v = merge_node(
                    inc.get(&k).cloned(),
                    run.get(&k),
                    file_map.and_then(|m| m.get(&k)),
                    path,
                    out,
                );
                path.pop();
                if let Some(v) = v {
                    merged.insert(k, v);
                }
            }
            Some(Value::Mapping(merged))
        }
        (incoming, _) => {
            if incoming.as_ref() != file {
                out.edited.push(path.clone());
            }
            incoming
        }
    }
}

fn is_leaf(v: Option<&Value>) -> bool {
    !matches!(v, Some(Value::Mapping(_)) | Some(Value::Sequence(_)))
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
        let found = unapply_echoes(&mut incoming, &running, &shadow);
        assert_eq!(incoming, y("secret: file-s\ncache: 50"));
        assert_eq!(found.edited, vec![slot(&["cache"])]);
        assert_eq!(found.echoed_over_file, vec![slot(&["secret"])]);
        // Echoing the FILE value (an import of an export) is not an edit.
        let mut incoming = y("secret: file-s\ncache: 10");
        assert_eq!(
            unapply_echoes(&mut incoming, &running, &shadow),
            Unapplied::default()
        );
        assert_eq!(
            display(&[
                SlotSeg::Key("backends".into()),
                SlotSeg::Named("eu".into()),
                SlotSeg::Key("key".into())
            ]),
            "backends[eu].key"
        );
    }

    #[test]
    fn composite_slots_are_unapplied_member_by_member() {
        // File: filesystem backend. Env: a whole S3 block with a secret.
        let file = y("backend: {type: filesystem, path: /file}");
        let running = y("backend: {type: s3, endpoint: http://env, region: us-east-1, secret_access_key: env-secret}");
        let shadow = capture(&file, &[slot(&["backend"])]);
        // The edit changes only the region.
        let mut incoming = y("backend: {type: s3, endpoint: http://env, region: eu-west-1, secret_access_key: env-secret}");
        let found = unapply_echoes(&mut incoming, &running, &shadow);
        let text = serde_yaml::to_string(&incoming).unwrap();
        assert!(!text.contains("env-secret"), "{text}");
        assert!(!text.contains("http://env"), "{text}");
        assert_eq!(
            get(&incoming, &slot(&["backend", "type"])),
            Some(&y("filesystem"))
        );
        assert_eq!(
            get(&incoming, &slot(&["backend", "path"])),
            Some(&y("/file"))
        );
        assert_eq!(found.edited, vec![slot(&["backend", "region"])]);

        // A null file value (no TLS block in the file): echoed members go.
        let shadow = capture(&y("tls: null"), &[slot(&["tls"])]);
        let running = y("tls: {enabled: true, cert_path: /env.pem}");
        let mut incoming = y("tls: {enabled: true, cert_path: /env.pem, key_path: /mine.pem}");
        unapply_echoes(&mut incoming, &running, &shadow);
        assert_eq!(incoming, y("tls: {key_path: /mine.pem}"));
    }
}
