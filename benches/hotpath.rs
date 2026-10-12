// SPDX-License-Identifier: BUSL-1.1

//! Criterion benchmarks for the fixed per-request CPU costs of the S3 path:
//! IAM authorization and the request principal.
//!
//! Every S3 request resolves a principal (the SigV4 middleware), hands it to
//! the authorization middleware and to the s3s adapter, and evaluates the
//! principal's policy at least once. These benches measure those steps
//! without the network, the signature or the storage.
//!
//! Run locally / manually (NOT part of the CI merge gate, like `codec`):
//!
//! ```bash
//! cargo bench --bench hotpath
//! ```
//!
//! Run them on a quiet machine: a loaded host moves the numbers more than
//! most changes do.

use criterion::{criterion_group, criterion_main, Criterion};
use deltaglider_proxy::iam::permissions::insert_source_ip;
use deltaglider_proxy::iam::{AuthenticatedUser, Permission, S3Action};
use std::hint::black_box;

fn perm(actions: &[&str], resources: &[&str], cond: Option<serde_json::Value>) -> Permission {
    Permission {
        id: 0,
        effect: "Allow".into(),
        actions: actions.iter().map(|s| s.to_string()).collect(),
        resources: resources.iter().map(|s| s.to_string()).collect(),
        conditions: cond,
    }
}

/// A user shaped like the production fixture: two plain grants, one
/// IP-conditioned grant, and `extra` plain grants on other buckets.
fn user(extra: usize) -> AuthenticatedUser {
    let mut permissions = vec![
        perm(
            &["read"],
            &["downloads/ci-uploader/*"],
            Some(serde_json::json!({"IpAddress": {"aws:SourceIp": ["10.0.0.0/8"]}})),
        ),
        perm(&["list", "write", "read"], &["releases/firmware/*"], None),
    ];
    for i in 0..extra {
        permissions.push(perm(&["read", "list"], &[&format!("bucket{i}/*")], None));
    }
    // The grant that allows the benched request comes last: the worst case
    // for a first-match scan.
    permissions.push(perm(
        &["read", "write", "list"],
        &["releases/reports/*"],
        None,
    ));
    AuthenticatedUser::new("ci-uploader", "AKCIUPLOADER", permissions)
}

fn authz(c: &mut Criterion) {
    let mut ctx = iam_rs::Context::new();
    insert_source_ip(&mut ctx, Some("10.1.2.3".parse().unwrap()));
    for extra in [0usize, 10] {
        let u = user(extra);
        let rules = u.permissions.len();
        c.bench_function(
            &format!("authz/get_allowed_by_last_rule/{rules}rules"),
            |b| {
                b.iter(|| {
                    black_box(u.can_with_context(
                        S3Action::Read,
                        black_box("releases"),
                        black_box("reports/2026/q3.pdf"),
                        &ctx,
                    ))
                })
            },
        );
        c.bench_function(&format!("authz/get_denied/{rules}rules"), |b| {
            b.iter(|| {
                black_box(u.can_with_context(
                    S3Action::Delete,
                    black_box("releases"),
                    black_box("reports/2026/q3.pdf"),
                    &ctx,
                ))
            })
        });
        c.bench_function(
            &format!("authz/get_allowed_by_condition/{rules}rules"),
            |b| {
                b.iter(|| {
                    black_box(u.can_with_context(
                        S3Action::Read,
                        black_box("downloads"),
                        black_box("ci-uploader/tool.tar.gz"),
                        &ctx,
                    ))
                })
            },
        );
    }
    let boot = AuthenticatedUser::bootstrap("AKBOOT");
    c.bench_function("authz/bootstrap_get", |b| {
        b.iter(|| {
            black_box(boot.can_with_context(
                S3Action::Read,
                black_box("releases"),
                black_box("reports/q3.pdf"),
                &ctx,
            ))
        })
    });
}

/// What one request pays to hand its principal to the next layer.
fn principal(c: &mut Criterion) {
    for extra in [0usize, 10] {
        let u = std::sync::Arc::new(user(extra));
        let rules = u.permissions.len();
        c.bench_function(&format!("principal/handoff/{rules}rules"), |b| {
            b.iter(|| black_box(std::sync::Arc::clone(black_box(&u))))
        });
    }
    c.bench_function("principal/bootstrap", |b| {
        b.iter(|| black_box(AuthenticatedUser::bootstrap_shared(black_box("AKBOOT"))))
    });
}

criterion_group!(benches, authz, principal);
criterion_main!(benches);
