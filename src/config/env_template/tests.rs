// SPDX-License-Identifier: BUSL-1.1

use super::*;
use crate::config::{expand_env_with, Config};
use proptest::prelude::*;

fn tpl(s: &str) -> Template {
    Template::parse(s).unwrap()
}

fn render_min(s: &str) -> String {
    tpl(s).render(Escape::Minimal)
}

// ── Templates ───────────────────────────────────────────────────────────

proptest! {
    /// The template reads a string as the expander does, writes it back so
    /// that the expander reads it the same way, in both escape modes.
    #[test]
    fn a_template_expands_like_the_expander_and_survives_a_render(
        s in proptest::collection::vec(
            prop_oneof![
                Just("$"), Just("{"), Just("}"), Just("env:"), Just("A"), Just(":-"),
                Just("x"), Just("${env:A}"), Just("${env:B:-d}"), Just("${iam:u}"), Just("$$"),
            ],
            0..10,
        ).prop_map(|v| v.concat())
    ) {
        let lookup = |n: &str| (n == "A").then(|| "VALUE".to_string());
        let direct = expand_env_with(&s, lookup);
        match Template::parse(&s) {
            Err(_) => prop_assert!(direct.is_err(), "{s:?}"),
            Ok(t) => {
                let via = t.expand(lookup);
                prop_assert_eq!(via.as_ref().ok(), direct.as_ref().ok(), "{:?}", s);
                if let Ok(want) = &direct {
                    for mode in [Escape::Full, Escape::Minimal] {
                        let text = t.render(mode);
                        let again = expand_env_with(&text, lookup);
                        prop_assert_eq!(again.as_ref().ok(), Some(want), "{:?} -> {:?}", s, text);
                        prop_assert_eq!(Template::parse(&text).unwrap(), t.clone(), "{:?}", text);
                    }
                }
            }
        }
    }

    /// A literal written by `escape_literal` is never a reference when read.
    #[test]
    fn an_escaped_literal_is_never_a_reference(
        s in proptest::collection::vec(
            prop_oneof![Just("$"), Just("{"), Just("}"), Just("env:"), Just("X"), Just("${env:X}")],
            0..10,
        ).prop_map(|v| v.concat())
    ) {
        for mode in [Escape::Full, Escape::Minimal] {
            let text = escape_literal(&s, mode, false);
            let t = Template::parse(&text).unwrap();
            prop_assert!(!t.has_refs(), "{:?} -> {:?}", s, text);
            prop_assert_eq!(t.expand(|_| None).unwrap(), s.clone());
        }
    }
}

#[test]
fn render_keeps_the_references_and_the_iam_templates_readable() {
    assert_eq!(
        render_min("arn:${iam:username}/${env:X}"),
        "arn:${iam:username}/${env:X}"
    );
    // `$$` is one literal `$`, which needs no doubling before a digit.
    assert_eq!(render_min("pay $$10"), "pay $10");
    assert_eq!(render_min("pay $$$$10"), "pay $$$10");
    assert_eq!(render_min("$${env:X}"), "$${env:X}");
    assert_eq!(tpl("a${env:X:-d}b").render(Escape::Full), "a${env:X:-d}b");
    // The same template, written two ways, is equal.
    assert_eq!(tpl("${iam:u}"), tpl("$${iam:u}"));
    assert!(tpl("${env:A}").is_single_ref() && !tpl("x${env:A}").is_single_ref());
}

// ── The path of a string ────────────────────────────────────────────────

fn labels(doc: &str) -> Vec<String> {
    let mut tree: Value = serde_yaml::from_str(doc).unwrap();
    struct Collect(Vec<String>);
    impl Visitor for Collect {
        fn string(&mut self, label: &str, _: &mut Value) -> Result<(), ConfigError> {
            self.0.push(label.to_string());
            Ok(())
        }
        fn key(&mut self, _: &str, _: &str) -> Result<Option<String>, ConfigError> {
            Ok(None)
        }
    }
    let shape = Shape::of(&tree);
    let mut c = Collect(Vec::new());
    walk(&mut tree, shape, &mut c).unwrap();
    c.0
}

#[test]
fn one_field_has_one_path_in_every_shape() {
    let sectioned = labels("access:\n  secret_access_key: a\n");
    let flat = labels("secret_access_key: a\n");
    assert_eq!(sectioned, ["/secret_access_key"]);
    assert_eq!(flat, sectioned);
    assert_eq!(
        labels("storage:\n  backends:\n  - name: hz\n    secret_access_key: a\n    other: b\n"),
        [
            "/backends/[hz]/name",
            "/backends/[hz]/secret_access_key",
            "/backends/[hz]/other"
        ]
    );
    // An item with no name is labelled by its index.
    assert_eq!(
        labels("advanced:\n  event_delivery:\n    webhook_urls: [a, b]\n"),
        [
            "/event_delivery/webhook_urls/#0",
            "/event_delivery/webhook_urls/#1"
        ]
    );
    // The storage shorthand names the backend field it sets.
    assert_eq!(
        labels("storage:\n  s3: https://e\n  region: r\n  secret_access_key: a\n"),
        [
            "/backend/endpoint",
            "/backend/region",
            "/backend/secret_access_key"
        ]
    );
    assert_eq!(
        labels("admission:\n  blocks:\n  - name: b1\n    match: {}\n    action: deny\n"),
        [
            "/admission_blocks/[b1]/name",
            "/admission_blocks/[b1]/action"
        ]
    );
}

// ── Recording at load ───────────────────────────────────────────────────

fn recorded(text: &str) -> BTreeMap<String, String> {
    record_templates(text)
        .unwrap()
        .into_iter()
        .map(|(p, t)| (p, t.render(Escape::Minimal)))
        .collect()
}

#[test]
fn the_template_of_every_reference_is_recorded_by_path() {
    let got = recorded(
        "access:\n  secret_access_key: ${env:S}\nadvanced:\n  public_url: \"https://h/${env:P}/x\"\n  log_level: ${env:L:-info}\n",
    );
    assert_eq!(got["/secret_access_key"], "${env:S}");
    assert_eq!(got["/public_url"], "https://h/${env:P}/x");
    assert_eq!(got["/log_level"], "${env:L:-info}");
    assert_eq!(got.len(), 3);
}

#[test]
fn a_comment_an_escaped_dollar_and_a_literal_record_nothing() {
    let got = recorded(
        "access:\n  # secret_access_key: ${env:S}\n  access_key_id: $${env:K}\nadvanced:\n  public_url: plain\n",
    );
    assert!(got.is_empty(), "{got:?}");
}

#[test]
fn a_reference_in_a_flow_collection_and_an_alias_are_recorded() {
    let got = recorded(
        "advanced:\n  event_delivery:\n    webhook_urls: [${env:A}, \"${env:B}\"]\n    webhook_headers: &h {X-A: ${env:A}}\n",
    );
    assert_eq!(got["/event_delivery/webhook_urls/#0"], "${env:A}");
    assert_eq!(got["/event_delivery/webhook_urls/#1"], "${env:B}");
    assert_eq!(got["/event_delivery/webhook_headers/X-A"], "${env:A}");
    // An alias copies the value to its own path.
    let aliased = recorded("access:\n  access_key_id: &k ${env:K}\n  secret_access_key: *k\n");
    assert_eq!(aliased["/access_key_id"], "${env:K}");
    assert_eq!(aliased["/secret_access_key"], "${env:K}");
}

/// The marker characters are chosen per text: a text that holds the
/// private-use characters is plain text, never a refusal and never a
/// marker.
#[test]
fn a_private_use_character_in_the_text_is_plain_text() {
    let text = "a: \"\u{E000}0\u{E001} ${env:X}\"\nb: \"\u{E002}\u{E003}\"\n";
    let got = record_templates(text).unwrap();
    let a = &got["/a"];
    assert_eq!(
        a.expand(|_| Some("v".to_string())).unwrap(),
        "\u{E000}0\u{E001} v"
    );
    assert!(!got.contains_key("/b"));
    // The same through the judge: the text reaches the value as it is.
    let cfg = Config::default();
    let (out, _) = admit(
        "advanced:\n  public_url: \"a\u{E000}0\u{E001}\u{E002}\"\n",
        &cfg,
    )
    .unwrap();
    assert!(out.contains("a\u{E000}0\u{E001}\u{E002}"), "{out}");
}

// ── The judge ───────────────────────────────────────────────────────────

const RUNNING: &str = "access:\n  secret_access_key: ${env:S}\nstorage:\n  backends:\n  - name: hz\n    type: s3\n    endpoint: https://s3.example.test\n    region: eu\n    secret_access_key: ${env:HZ}\nadvanced:\n  public_url: https://h/${env:P}\n";

fn running() -> Config {
    let env = |n: &str| match n {
        "S" => Some("secret-S-000000000001".to_string()),
        "HZ" => Some("secret-HZ-00000000001".to_string()),
        "P" => Some("path-token-0000000001".to_string()),
        _ => None,
    };
    Config::from_template_with(RUNNING, &env).unwrap()
}

fn admit(doc: &str, cfg: &Config) -> Result<(String, EnvRefs), String> {
    admit_with(doc, cfg, &|_| false)
}

fn admit_with(
    doc: &str,
    cfg: &Config,
    allowed: &dyn Fn(&str) -> bool,
) -> Result<(String, EnvRefs), String> {
    let env = |n: &str| (n == "ALLOWED_TOKEN").then(|| "allowed-secret-00001".to_string());
    admit_document(doc, &cfg.env_refs, allowed, &env)
        .map(|(tree, added)| (serde_yaml::to_string(&tree).unwrap(), added))
        .map_err(|e| e.to_string())
}

#[test]
fn a_reference_is_admitted_at_its_own_path_with_its_own_text() {
    let cfg = running();
    let (text, added) = admit(RUNNING, &cfg).unwrap();
    assert!(added.is_empty());
    assert!(text.contains("secret-S-000000000001"), "{text}");
    assert!(text.contains("https://h/path-token-0000000001"), "{text}");
    // A different document shape of the same fields is the same paths.
    let flat = "secret_access_key: ${env:S}\npublic_url: https://h/${env:P}\n";
    assert!(admit(flat, &cfg).is_ok());
    // Written in a flow, quoted, or with `$$` elsewhere in the text: same
    // templates.
    assert!(admit("access: {secret_access_key: \"${env:S}\"}\n", &cfg).is_ok());
}

#[test]
fn a_reference_anywhere_else_is_refused_with_the_path_and_never_a_value() {
    let cfg = running();
    let secret = "secret-S-000000000001";
    for (doc, path) in [
        // The documented attack: the secret inside another field.
        ("advanced:\n  public_url: http://h.example/${env:S}\n", "/public_url"),
        // A whole reference in another field.
        ("advanced:\n  public_url: ${env:S}\n", "/public_url"),
        // Another recorded name at a recorded path.
        ("access:\n  secret_access_key: ${env:HZ}\n", "/secret_access_key"),
        // The embedded text differs.
        ("advanced:\n  public_url: https://other/${env:P}\n", "/public_url"),
        // Two references in one string.
        ("access:\n  secret_access_key: ${env:S}${env:HZ}\n", "/secret_access_key"),
        // A renamed list item.
        (
            "storage:\n  backends:\n  - name: hz2\n    type: s3\n    secret_access_key: ${env:HZ}\n",
            "/backends/[hz2]/secret_access_key",
        ),
        // A name no file recorded, with or without a default.
        ("advanced:\n  log_level: ${env:HOME}\n", "/log_level"),
        ("advanced:\n  log_level: ${env:HOME:-info}\n", "/log_level"),
        // A numeric field.
        ("advanced:\n  cache_size_mb: ${env:S}\n", "/cache_size_mb"),
        // A list item, a header value, a map key.
        (
            "advanced:\n  event_delivery:\n    webhook_headers:\n      X-A: ${env:S}\n",
            "/event_delivery/webhook_headers/X-A",
        ),
        (
            "advanced:\n  event_delivery:\n    webhook_headers:\n      \"${env:S}\": v\n",
            "/event_delivery/webhook_headers/",
        ),
    ] {
        let err = admit(doc, &cfg).expect_err(doc);
        assert!(err.contains(path), "{doc}\n{err}");
        assert!(
            err.contains("refused") || err.contains("comes from the environment"),
            "{err}"
        );
        assert!(!err.contains(secret) && !err.contains("path-token"), "{err}");
    }
}

#[test]
fn an_alias_cannot_copy_an_accepted_reference_to_another_path() {
    let cfg = running();
    let doc = "access:\n  secret_access_key: &s ${env:S}\nadvanced:\n  event_delivery:\n    webhook_headers:\n      X-Leak: *s\n";
    let err = admit(doc, &cfg).unwrap_err();
    assert!(
        err.contains("/event_delivery/webhook_headers/X-Leak"),
        "{err}"
    );
    // A merge key copies it too.
    let doc = "access: &a\n  secret_access_key: ${env:S}\nstorage:\n  <<: *a\n";
    assert!(admit(doc, &cfg).is_err());
}

#[test]
fn an_escaped_reference_is_a_literal_and_text_that_is_not_a_reference_passes() {
    let cfg = running();
    let (text, added) = admit(
        "advanced:\n  config_sync_object_key: \"https://h/$${env:S}\"\n  log_level: info\n",
        &cfg,
    )
    .unwrap();
    assert!(
        text.contains("https://h/${env:S}") && !text.contains("secret-S"),
        "{text}"
    );
    assert!(added.is_empty());
    // A bcrypt hash and an IAM template are not references.
    let doc = "advanced:\n  bootstrap_password_hash: \"$2b$04$abcdefghijklmnopqrstuuABCDEFGHIJKLMNOPQRSTUVWXYZ01234\"\naccess:\n  iam_users:\n  - name: u\n    access_key_id: AK\n    secret_access_key: s\n    permissions:\n    - effect: Allow\n      actions: [read]\n      resources: [\"b/${iam:username}/*\"]\n";
    assert!(admit(doc, &cfg).is_ok());
}

#[test]
fn text_with_a_broken_reference_is_refused() {
    let cfg = running();
    assert!(admit("advanced:\n  public_url: \"a${env:X\"\n", &cfg).is_err());
    // Two documents in one body.
    assert!(admit("a: 1\n---\nb: 2\n", &cfg).is_err());
}

proptest! {
    /// A document with any reference at a path that has no template is
    /// refused, whatever the name, the field or the text around it.
    #[test]
    fn any_reference_at_an_unrecorded_path_is_refused(
        name in "[A-Z][A-Z0-9_]{0,12}",
        before in "[a-z/:. ]{0,8}",
        after in "[a-z/:. ]{0,8}",
        default in proptest::option::of("[a-z]{0,4}"),
        field in prop_oneof![
            Just("log_level"), Just("public_url"), Just("config_sync_bucket"),
            Just("config_sync_object_key"),
        ],
    ) {
        let reference = match default {
            Some(d) => format!("${{env:{name}:-{d}}}"),
            None => format!("${{env:{name}}}"),
        };
        let doc = format!("advanced:\n  {field}: \"{before}{reference}{after}\"\n");
        let empty = Config::default();
        prop_assert!(admit(&doc, &empty).is_err(), "{doc}");
        // Even when another path records the same name.
        prop_assert!(admit(&doc, &running()).is_err(), "{doc}");
    }

    /// A string with no reference passes and expands to itself, whatever
    /// the dollars in it.
    #[test]
    fn a_string_without_a_reference_expands_to_its_unescaped_self(
        s in proptest::collection::vec(prop_oneof![Just("$$"), Just("a"), Just(" "), Just("$1"), Just("${iam:x}")], 0..6)
            .prop_map(|v| v.concat())
    ) {
        let doc = format!("advanced:\n  config_sync_object_key: {}\n", serde_json::to_string(&s).unwrap());
        let (text, _) = admit(&doc, &Config::default()).unwrap();
        let tree: Value = serde_yaml::from_str(&text).unwrap();
        let got = tree["advanced"]["config_sync_object_key"].as_str().unwrap().to_string();
        prop_assert_eq!(got, expand_env_with(&s, |_| None).unwrap());
    }
}

// ── The allowlist ───────────────────────────────────────────────────────

#[test]
fn an_allowlisted_name_is_accepted_whole_in_a_backend_secret_only() {
    let cfg = Config::default();
    let allowed = |n: &str| n == "ALLOWED_TOKEN";
    let slot = "storage:\n  backends:\n  - name: b\n    type: s3\n    endpoint: https://e.example\n    region: eu\n    secret_access_key: ${env:ALLOWED_TOKEN}\n";
    let (text, added) = admit_with(slot, &cfg, &allowed).unwrap();
    assert!(text.contains("allowed-secret-00001"), "{text}");
    assert_eq!(
        added.get("ALLOWED_TOKEN").map(String::as_str),
        Some("allowed-secret-00001")
    );
    assert_eq!(
        added.templates["/backends/[b]/secret_access_key"].render(Escape::Minimal),
        "${env:ALLOWED_TOKEN}",
        "the new template lets persist and export write the reference"
    );
    assert!(
        added.frozen.is_empty(),
        "the allowlist never adds to the frozen set"
    );
    for (doc, path) in [
        // A field that an export shows in clear.
        ("advanced:\n  public_url: ${env:ALLOWED_TOKEN}\n", "/public_url"),
        // Inside a longer text, even in a backend secret.
        (
            "storage:\n  backends:\n  - name: b\n    type: s3\n    secret_access_key: x${env:ALLOWED_TOKEN}\n",
            "/backends/[b]/secret_access_key",
        ),
        // A name that is not allowlisted, in a backend secret.
        (
            "storage:\n  backends:\n  - name: b\n    type: s3\n    secret_access_key: ${env:OTHER}\n",
            "/backends/[b]/secret_access_key",
        ),
        // A backend's key id: the backend list shows it.
        (
            "storage:\n  backends:\n  - name: b\n    type: s3\n    access_key_id: ${env:ALLOWED_TOKEN}\n",
            "/backends/[b]/access_key_id",
        ),
        // The proxy's own SigV4 pair.
        ("access:\n  secret_access_key: ${env:ALLOWED_TOKEN}\n", "/secret_access_key"),
        ("access:\n  access_key_id: ${env:ALLOWED_TOKEN}\n", "/access_key_id"),
        // A header value and a provider secret: sent to a URL an admin sets.
        (
            "advanced:\n  event_delivery:\n    webhook_headers:\n      X-A: ${env:ALLOWED_TOKEN}\n",
            "/event_delivery/webhook_headers/X-A",
        ),
        (
            "access:\n  auth_providers:\n  - name: p\n    provider_type: oidc\n    client_secret: ${env:ALLOWED_TOKEN}\n",
            "/auth_providers/[p]/client_secret",
        ),
        // The Slack token.
        (
            "advanced:\n  event_delivery:\n    slack_bot_token: ${env:ALLOWED_TOKEN}\n",
            "/event_delivery/slack_bot_token",
        ),
        // A backend's endpoint.
        (
            "storage:\n  backends:\n  - name: b\n    type: s3\n    endpoint: ${env:ALLOWED_TOKEN}\n",
            "/backends/[b]/endpoint",
        ),
    ] {
        let err = admit_with(doc, &cfg, &allowed).expect_err(doc);
        assert!(err.contains(path) && !err.contains("allowed-secret"), "{doc}\n{err}");
    }
}

#[test]
fn the_allowlist_slots_are_the_fields_that_nothing_reads_or_sends() {
    for yes in [
        "/backend/secret_access_key",
        "/backends/[b]/secret_access_key",
        "/backends/[b]/encryption/key",
        "/backends/[b]/encryption/legacy_key",
        "/backend_encryption/key",
        "/backend_encryption/legacy_key",
        "/buckets/b/encryption/key",
    ] {
        assert!(is_allowlist_slot(yes), "{yes}");
    }
    for no in [
        "/secret_access_key",
        "/access_key_id",
        "/backend/access_key_id",
        "/backends/[b]/access_key_id",
        "/iam_users/[u]/secret_access_key",
        "/auth_providers/[p]/client_secret",
        "/event_delivery/slack_bot_token",
        "/event_delivery/webhook_headers/Authorization",
        "/public_url",
        "/log_level",
        "/event_delivery/webhook_url",
        "/backends/[b]/endpoint",
        "/config_sync_object_key",
        "/listen_addr",
    ] {
        assert!(!is_allowlist_slot(no), "{no}");
    }
}

// ── The file text ───────────────────────────────────────────────────────

#[test]
fn a_template_is_written_only_while_the_value_is_what_it_expands_to() {
    let cfg = running();
    let refs = &cfg.env_refs;
    let p = "/public_url";
    assert_eq!(
        file_text(p, "https://h/path-token-0000000001", refs, Escape::Minimal),
        "https://h/${env:P}"
    );
    // Changed: the new value, escaped.
    assert_eq!(
        file_text(p, "https://x/$y", refs, Escape::Full),
        "https://x/$$y"
    );
    // Another path: never.
    assert_eq!(
        file_text(
            "/log_level",
            "https://h/path-token-0000000001",
            refs,
            Escape::Minimal
        ),
        "https://h/path-token-0000000001"
    );
}

// ── The frozen set ──────────────────────────────────────────────────────

fn tree_of(cfg: &Config) -> Value {
    serde_yaml::to_value(cfg).unwrap()
}

/// `frozen_violation` of a change of the config `old` into `new`.
fn violation(old: &Config, new: &Config) -> Option<String> {
    frozen_violation(&old.env_refs, &tree_of(old), &tree_of(new), false)
}

const EVENTS: &str = "advanced:\n  event_delivery:\n    enabled: true\n    webhook_url: https://hooks.example/in\n    webhook_headers:\n      Authorization: Bearer ${env:HOOK_TOKEN}\n      X-Literal: plain\n";
const OIDC: &str = "access:\n  iam_mode: declarative\n  auth_providers:\n  - name: corp\n    provider_type: oidc\n    client_id: dgp\n    client_secret: ${env:OIDC_SECRET}\n    issuer_url: https://id.example.com\n";

fn env_for_destinations(n: &str) -> Option<String> {
    Some(format!("value-of-{}-0000", n.to_lowercase()))
}

#[test]
fn the_destinations_of_a_frozen_credential_are_frozen_with_it() {
    let cfg = Config::from_template_with(EVENTS, &env_for_destinations).unwrap();
    let frozen = &cfg.env_refs.frozen;
    assert_eq!(
        frozen.templates.keys().collect::<Vec<_>>(),
        ["/event_delivery/webhook_headers/Authorization"]
    );
    assert_eq!(
        frozen
            .destinations
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "/event_delivery/allow_local",
            "/event_delivery/webhook_url",
            "/event_delivery/webhook_urls"
        ]
    );
    // A file with a literal credential freezes nothing.
    let literal = Config::from_template_with(
        &EVENTS.replace("${env:HOOK_TOKEN}", "tok"),
        &env_for_destinations,
    )
    .unwrap();
    assert!(literal.env_refs.frozen.is_empty() && literal.env_refs.frozen.destinations.is_empty());
}

#[test]
fn a_frozen_field_and_a_destination_of_its_credential_cannot_change() {
    // (document, a change of one field, the path the message names)
    type Edit = fn(&mut Config);
    let cases: [(&str, Edit, &str); 9] = [
        (
            EVENTS,
            |c| c.event_delivery.webhook_url = Some("https://evil.example/in".into()),
            "/event_delivery/webhook_url",
        ),
        (
            EVENTS,
            |c| {
                c.event_delivery
                    .webhook_urls
                    .push("https://evil.example/2".into())
            },
            "/event_delivery/webhook_urls",
        ),
        (
            EVENTS,
            |c| c.event_delivery.allow_local = true,
            "/event_delivery/allow_local",
        ),
        (
            EVENTS,
            |c| {
                c.event_delivery
                    .webhook_headers
                    .insert("Authorization".into(), "Bearer typed".into());
            },
            "/event_delivery/webhook_headers/Authorization",
        ),
        (
            EVENTS,
            |c| {
                c.event_delivery.webhook_headers.remove("Authorization");
            },
            "/event_delivery/webhook_headers/Authorization",
        ),
        (
            OIDC,
            |c| c.auth_providers[0].issuer_url = Some("https://evil.example".into()),
            "/auth_providers/[corp]/issuer_url",
        ),
        (
            OIDC,
            |c| c.auth_providers[0].extra_config = Some(serde_json::json!({"allow_local": true})),
            "/auth_providers/[corp]/extra_config",
        ),
        // The object goes, or goes under another name.
        (
            OIDC,
            |c| c.auth_providers.clear(),
            "/auth_providers/[corp]/client_secret",
        ),
        (
            OIDC,
            |c| c.auth_providers[0].name = "corp2".into(),
            "/auth_providers/[corp]/client_secret",
        ),
    ];
    for (doc, edit, path) in cases {
        let old = Config::from_template_with(doc, &env_for_destinations).unwrap();
        let mut new = old.clone();
        edit(&mut new);
        let err = violation(&old, &new).expect(path);
        assert!(
            err.contains(path) && err.contains("comes from the environment"),
            "{err}"
        );
        assert!(err.contains("config file and restart"), "{err}");
        assert!(!err.contains("value-of-"), "{err}");
        // The control: the same change when the credential is a literal.
        let literal = Config::from_template_with(
            &doc.replace("${env:HOOK_TOKEN}", "tok")
                .replace("${env:OIDC_SECRET}", "sec"),
            &env_for_destinations,
        )
        .unwrap();
        let mut moved = literal.clone();
        edit(&mut moved);
        assert_eq!(violation(&literal, &moved), None, "{path}");
        // And no change at all passes.
        assert_eq!(violation(&old, &old), None, "{path}");
    }
}

#[test]
fn an_unrelated_change_next_to_a_frozen_field_passes() {
    let old = Config::from_template_with(EVENTS, &env_for_destinations).unwrap();
    let mut new = old.clone();
    new.event_delivery.batch_size = 7;
    new.event_delivery
        .webhook_headers
        .insert("X-Literal".into(), "typed by the admin".into());
    new.event_delivery
        .webhook_headers
        .insert("X-New".into(), "also typed".into());
    assert_eq!(violation(&old, &new), None);
}

#[test]
fn the_declarative_mode_cannot_be_left_while_the_iam_state_holds_a_frozen_field() {
    let old = Config::from_template_with(OIDC, &env_for_destinations).unwrap();
    let mut gui = old.clone();
    gui.iam_mode = crate::config_sections::IamMode::Gui;
    let err = violation(&old, &gui).unwrap();
    assert!(
        err.contains("/iam_mode") && err.contains("/auth_providers/[corp]/client_secret"),
        "{err}"
    );
    assert!(err.contains("comes from the environment"), "{err}");
    // A frozen field outside the IAM state does not tie the mode.
    let events = Config::from_template_with(
        &format!("access:\n  iam_mode: declarative\n{}", EVENTS),
        &env_for_destinations,
    )
    .unwrap();
    let mut gui = events.clone();
    gui.iam_mode = crate::config_sections::IamMode::Gui;
    assert_eq!(violation(&events, &gui), None);
}

#[test]
fn a_config_with_no_reference_has_an_empty_frozen_set() {
    let cfg = Config::from_template_with(
        "access:\n  secret_access_key: plain\nadvanced:\n  public_url: https://h/$${env:X}\n",
        &env_for_destinations,
    )
    .unwrap();
    assert!(cfg.env_refs.frozen.is_empty());
    let mut other = cfg.clone();
    other.public_url = Some("https://elsewhere/".into());
    assert_eq!(violation(&cfg, &other), None);
}

// ── The frozen set holds whatever the requests are ──────────────────────

const BOOT: &str = "\
access:
  access_key_id: AKBOOT
  secret_access_key: ${env:S}
  iam_mode: declarative
  iam_users:
  - name: dana
    access_key_id: AKDANA
    secret_access_key: dana-literal-secret
    permissions:
    - id: 0
      effect: Allow
      actions: [read]
      resources: [\"*\"]
  auth_providers:
  - name: corp
    provider_type: oidc
    client_id: dgp
    client_secret: ${env:OIDC}
    issuer_url: https://id.example.com
storage:
  backend:
    type: filesystem
    path: ./data
advanced:
  cache_size_mb: ${env:CACHE}
  public_url: https://h/${env:P}
  event_delivery:
    enabled: true
    webhook_url: https://hooks.example/in
    webhook_headers:
      Authorization: Bearer ${env:HOOK}
";

fn boot_env(name: &str) -> Option<String> {
    Some(match name {
        "CACHE" => "256".to_string(),
        other => format!("value-of-{other}-00000000"),
    })
}

/// One edit of an exported document: where, and what to put there.
#[derive(Debug, Clone)]
enum Edit {
    Set(usize, usize),
    Remove(usize),
    Gui,
    DropProviders,
}

/// The paths of the document that the edits touch.
const PATHS: [&[&str]; 9] = [
    &["access", "secret_access_key"],
    &["advanced", "public_url"],
    &["advanced", "cache_size_mb"],
    &["advanced", "event_delivery", "webhook_url"],
    &[
        "advanced",
        "event_delivery",
        "webhook_headers",
        "Authorization",
    ],
    &["advanced", "event_delivery", "webhook_headers", "X-New"],
    &["advanced", "event_delivery", "allow_local"],
    &["advanced", "log_level"],
    &["access", "iam_users"],
];

const PUT_VALUES: [&str; 8] = [
    "typed-by-the-admin",
    "${env:S}",
    "${env:HOOK}",
    "${env:HOME}",
    "https://evil.example/x",
    "Bearer ${env:HOOK}",
    "https://h/${env:P}",
    "$${env:S}",
];

fn at_path<'a>(doc: &'a mut Value, path: &[&str]) -> Option<&'a mut Value> {
    path.iter()
        .try_fold(doc, |v, k| v.as_mapping_mut()?.get_mut(*k))
}

fn apply_edit(doc: &mut Value, edit: &Edit) {
    match edit {
        Edit::Set(p, v) => {
            let path = PATHS[*p];
            let (last, parents) = path.split_last().unwrap();
            let mut at = &mut *doc;
            for k in parents {
                let map = at.as_mapping_mut().unwrap();
                at = map
                    .entry(Value::String((*k).to_string()))
                    .or_insert_with(|| Value::Mapping(Default::default()));
            }
            if let Some(map) = at.as_mapping_mut() {
                let text = PUT_VALUES[*v];
                // A plain string, or the number that the field may hold.
                let value = if *last == "allow_local" {
                    Value::Bool(*v % 2 == 0)
                } else {
                    Value::String(text.to_string())
                };
                map.insert(Value::String((*last).to_string()), value);
            }
        }
        Edit::Remove(p) => {
            let path = PATHS[*p];
            let (last, parents) = path.split_last().unwrap();
            if let Some(map) = at_path(doc, parents).and_then(Value::as_mapping_mut) {
                map.remove(*last);
            }
        }
        Edit::Gui => {
            if let Some(access) = at_path(doc, &["access"]).and_then(Value::as_mapping_mut) {
                access.insert("iam_mode".into(), "gui".into());
            }
        }
        Edit::DropProviders => {
            if let Some(access) = at_path(doc, &["access"]).and_then(Value::as_mapping_mut) {
                access.insert("auth_providers".into(), Value::Sequence(Vec::new()));
            }
        }
    }
}

/// The document surface, end to end and pure: export the running config,
/// edit it, judge every string, load it as a typed config, compare it with
/// the running one over the frozen set. `Some(config)` when it is accepted.
fn try_edit(running: &Config, edit: &Edit) -> Option<Config> {
    let exported = running.redact_all_secrets().to_canonical_yaml().ok()?;
    let mut doc: Value = serde_yaml::from_str(&exported).ok()?;
    apply_edit(&mut doc, edit);
    let text = serde_yaml::to_string(&doc).ok()?;
    let (tree, added) = admit_document(&text, &running.env_refs, &|_| false, &|_| None).ok()?;
    let mut new = Config::from_yaml_str(&serde_yaml::to_string(&tree).ok()?).ok()?;
    let mut refs = running.env_refs.clone();
    refs.merge(added);
    new.env_refs = refs;
    // The masked secrets that an export hides come back from the running
    // config, as the write pipeline does.
    new.bootstrap_password_hash = running.bootstrap_password_hash.clone();
    if frozen_violation(&running.env_refs, &tree_of(running), &tree_of(&new), false).is_some() {
        return None;
    }
    Some(new)
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (0..PATHS.len(), 0..PUT_VALUES.len()).prop_map(|(p, v)| Edit::Set(p, v)),
        (0..PATHS.len()).prop_map(Edit::Remove),
        Just(Edit::Gui),
        Just(Edit::DropProviders),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// After any sequence of accepted edits, every frozen path still holds
    /// its boot value, the persisted text holds a reference only where the
    /// file had it, and the persisted file loads to the same frozen values.
    #[test]
    fn every_frozen_path_keeps_its_boot_state_after_any_sequence_of_edits(
        edits in proptest::collection::vec(edit_strategy(), 1..7)
    ) {
        let boot = Config::from_template_with(BOOT, &boot_env).unwrap();
        let boot_tree = tree_of(&boot);
        let mut boot_nodes = BTreeMap::new();
        index_nodes(&boot_tree, &mut String::new(), &mut boot_nodes);
        let frozen = boot.env_refs.frozen.clone();
        prop_assert!(frozen.templates.len() >= 5, "{:?}", frozen.templates.keys());
        let mut running = boot.clone();
        for edit in &edits {
            if let Some(next) = try_edit(&running, edit) {
                running = next;
            }
            let tree = tree_of(&running);
            let mut nodes = BTreeMap::new();
            index_nodes(&tree, &mut String::new(), &mut nodes);
            for label in frozen.templates.keys().map(String::as_str).chain(frozen.destinations.iter().map(String::as_str)) {
                prop_assert_eq!(nodes.get(label), boot_nodes.get(label), "{} after {:?}", label, edits);
            }
            prop_assert_eq!(&running.env_refs.frozen, &frozen);
            // What a restart reads.
            let text = running
                .to_canonical_yaml_for_persist_with(&|_| None)
                .map_err(|e| TestCaseError::fail(format!("{e}")))?;
            let reloaded = Config::from_template_with(&text, &boot_env)
                .map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?;
            prop_assert_eq!(&reloaded.env_refs.frozen.templates, &frozen.templates, "{}", text);
        }
    }
}

// ── The last gate and the typed fields ──────────────────────────────────

#[test]
fn the_text_gate_refuses_a_reference_that_the_file_does_not_have_there() {
    let cfg = running();
    let refs = &cfg.env_refs;
    // The file's own references pass.
    let own =
        "access:\n  secret_access_key: ${env:S}\nadvanced:\n  public_url: https://h/${env:P}\n";
    assert!(refuse_unrecorded_refs(own, refs).is_ok());
    for (text, label) in [
        (
            "advanced:\n  public_url: https://h/${env:S}\n",
            "/public_url",
        ),
        ("advanced:\n  log_level: ${env:S}\n", "/log_level"),
        (
            "advanced:\n  event_delivery:\n    webhook_headers:\n      \"${env:S}\": v\n",
            "/event_delivery/webhook_headers/",
        ),
    ] {
        let err = refuse_unrecorded_refs(text, refs)
            .expect_err(text)
            .to_string();
        assert!(
            err.contains(label) && err.contains("refusing to write"),
            "{err}"
        );
        assert!(!err.contains("secret-S"), "{err}");
    }
    // An escaped reference is text.
    assert!(refuse_unrecorded_refs("advanced:\n  log_level: $${env:S}\n", refs).is_ok());
}

#[test]
fn a_typed_field_gets_its_template_back_in_the_text() {
    let cfg = Config::from_template_with(BOOT, &boot_env).unwrap();
    let text = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(text.contains("cache_size_mb: ${env:CACHE}"), "{text}");
    assert!(
        text.contains("${env:S}") && !text.contains("value-of-S"),
        "{text}"
    );
    let again = Config::from_template_with(&text, &boot_env).unwrap();
    assert_eq!(again.cache_size_mb, 256);
    assert_eq!(
        again.env_refs.frozen.templates,
        cfg.env_refs.frozen.templates
    );
}

#[test]
fn a_typed_frozen_field_may_come_back_as_its_value_but_a_credential_may_not() {
    let cfg = Config::from_template_with(
        "access:\n  iam_mode: ${env:MODE}\n  secret_access_key: ${env:S}\n",
        &|n| match n {
            "MODE" => Some("declarative".to_string()),
            "S" => Some("s3cr3t-value-0001".to_string()),
            _ => None,
        },
    )
    .unwrap();
    // A GET shows the enum as its value: the same value is accepted.
    let (text, _) = admit("access:\n  iam_mode: declarative\n", &cfg).unwrap();
    assert!(text.contains("declarative"), "{text}");
    // Another value, even a valid one, is not the field's value.
    assert!(admit("access:\n  iam_mode: gui\n", &cfg).is_err());
    // A credential never comes back as a value: the right guess is refused
    // like a wrong one.
    for guess in ["s3cr3t-value-0001", "wrong-guess"] {
        let doc = format!("access:\n  secret_access_key: {guess}\n");
        let err = admit(&doc, &cfg).unwrap_err();
        assert!(err.contains("/secret_access_key"), "{err}");
        assert!(err.contains("comes from the environment"), "{err}");
        assert!(!err.contains("s3cr3t"), "{err}");
    }
}

// ── The CLI document ────────────────────────────────────────────────────

#[test]
fn the_cli_keeps_the_references_that_the_server_has_and_expands_the_rest() {
    let server = "access:\n  secret_access_key: ${env:S}\nadvanced:\n  public_url: https://h/${env:P}\n  cache_size_mb: ${env:CACHE}\n";
    let operator = "access:\n  access_key_id: ${env:AK}\n  secret_access_key: ${env:S}\nadvanced:\n  public_url: https://h/${env:P}\n  cache_size_mb: ${env:CACHE}\n  config_sync_object_key: \"pay $$10 ${env:OTHER}\"\n";
    let env = |n: &str| {
        Some(match n {
            "AK" => "AKOPERATOR".to_string(),
            "CACHE" => "512".to_string(),
            "OTHER" => "a${env:X}b".to_string(),
            // The server's own names resolve to something else on the
            // operator's host: they must not be sent.
            "S" => "operator-secret-0001".to_string(),
            "P" => "operator-path-0001".to_string(),
            _ => return None,
        })
    };
    let doc = cli_document(operator, server, &env).unwrap();
    // The references of the server stay references: nothing of the
    // operator's values for them is sent.
    assert!(
        doc.contains("${env:S}") && doc.contains("https://h/${env:P}"),
        "{doc}"
    );
    assert!(doc.contains("${env:CACHE}"), "{doc}");
    assert!(
        !doc.contains("operator-secret") && !doc.contains("operator-path"),
        "{doc}"
    );
    // The others are expanded, escaped, and a whole one is typed by its field.
    assert!(doc.contains("AKOPERATOR"), "{doc}");
    assert!(doc.contains("pay $$10 a$${env:X}b"), "{doc}");
    // The server accepts it as it is.
    let cfg = Config::from_template_with(server, &|n| {
        Some(match n {
            "S" => "boot-secret-00000001".to_string(),
            "P" => "boot-path-000000001".to_string(),
            "CACHE" => "256".to_string(),
            _ => return None,
        })
    })
    .unwrap();
    let (tree, _) = admit_document(&doc, &cfg.env_refs, &|_| false, &|_| None).unwrap();
    let loaded = Config::from_yaml_str(&serde_yaml::to_string(&tree).unwrap()).unwrap();
    assert_eq!(
        loaded.secret_access_key.as_deref(),
        Some("boot-secret-00000001")
    );
    assert_eq!(loaded.access_key_id.as_deref(), Some("AKOPERATOR"));
    assert_eq!(loaded.cache_size_mb, 256);
    assert_eq!(
        loaded.config_sync_object_key.as_deref(),
        Some("pay $10 a${env:X}b")
    );
}

#[test]
fn the_cli_fails_on_an_unset_variable_that_the_server_does_not_have() {
    let err = cli_document("advanced:\n  log_level: ${env:NOPE}\n", "", &|_| None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("NOPE"), "{err}");
}
