// SPDX-License-Identifier: BUSL-1.1

//! Single integration-test binary.
//!
//! Every `tests/*.rs` file is a module of this crate instead of its own test
//! binary, so CI links once instead of ~70 times (less memory, far less disk,
//! faster test runs). Select a file's tests with a libtest filter, e.g.
//! `cargo test --test all -- s3_api_test::`.
//! `scripts/check-integration-tests-in-ci.sh` enforces that every file is
//! listed here and selected by a filter in `.github/workflows/ci.yml`.

#[macro_use]
mod common;

#[path = "admin_bulk_objects_test.rs"]
mod admin_bulk_objects_test;
#[path = "admin_config_test.rs"]
mod admin_config_test;
#[path = "admin_login_as_test.rs"]
mod admin_login_as_test;
#[path = "admin_section_test.rs"]
mod admin_section_test;
#[path = "admission_test.rs"]
mod admission_test;
#[path = "auth_integration_test.rs"]
mod auth_integration_test;
#[path = "aws_chunked_upload_test.rs"]
mod aws_chunked_upload_test;
#[path = "backend_capability_gate_test.rs"]
mod backend_capability_gate_test;
#[path = "backend_health_test.rs"]
mod backend_health_test;
#[path = "bootstrap_password_hash_invariant_test.rs"]
mod bootstrap_password_hash_invariant_test;
#[path = "browser_session_connect_test.rs"]
mod browser_session_connect_test;
#[path = "bucket_existence_test.rs"]
mod bucket_existence_test;
#[path = "bucket_usage_test.rs"]
mod bucket_usage_test;
#[path = "cli_admin_test.rs"]
mod cli_admin_test;
#[path = "cli_s3_bucket_acl_test.rs"]
mod cli_s3_bucket_acl_test;
#[path = "cli_s3_cp_test.rs"]
mod cli_s3_cp_test;
#[path = "cli_s3_ls_test.rs"]
mod cli_s3_ls_test;
#[path = "cli_s3_migrate_test.rs"]
mod cli_s3_migrate_test;
#[path = "cli_s3_purge_test.rs"]
mod cli_s3_purge_test;
#[path = "cli_s3_rm_test.rs"]
mod cli_s3_rm_test;
#[path = "cli_s3_stats_test.rs"]
mod cli_s3_stats_test;
#[path = "cli_s3_sync_test.rs"]
mod cli_s3_sync_test;
#[path = "cli_s3_verify_test.rs"]
mod cli_s3_verify_test;
#[path = "concurrency_test.rs"]
mod concurrency_test;
#[path = "config_sync_ha_test.rs"]
mod config_sync_ha_test;
#[path = "config_sync_test.rs"]
mod config_sync_test;
#[path = "coordination_lease_test.rs"]
mod coordination_lease_test;
#[path = "delta_passthrough_test.rs"]
mod delta_passthrough_test;
#[path = "delta_test.rs"]
mod delta_test;
#[path = "encryption_test.rs"]
mod encryption_test;
#[path = "error_test.rs"]
mod error_test;
#[path = "external_auth_test.rs"]
mod external_auth_test;
#[path = "folder_marker_test.rs"]
mod folder_marker_test;
#[path = "generation_pin_test.rs"]
mod generation_pin_test;
#[path = "harness_port_test.rs"]
mod harness_port_test;
#[path = "iam_authorization_test.rs"]
mod iam_authorization_test;
#[path = "iam_declarative_reconcile_test.rs"]
mod iam_declarative_reconcile_test;
#[path = "iam_identity_test.rs"]
mod iam_identity_test;
#[path = "iam_list_scope_test.rs"]
mod iam_list_scope_test;
#[path = "iam_mode_test.rs"]
mod iam_mode_test;
#[path = "iam_persona_test.rs"]
mod iam_persona_test;
#[path = "iam_test.rs"]
mod iam_test;
#[path = "iam_variables_clone_test.rs"]
mod iam_variables_clone_test;
#[path = "large_object_e2e_test.rs"]
mod large_object_e2e_test;
#[path = "lifecycle_test.rs"]
mod lifecycle_test;
#[path = "list_truncation_test.rs"]
mod list_truncation_test;
#[path = "logs_test.rs"]
mod logs_test;
#[path = "maintenance_backfill_test.rs"]
mod maintenance_backfill_test;
#[path = "maintenance_reencrypt_test.rs"]
mod maintenance_reencrypt_test;
#[path = "memory_test.rs"]
mod memory_test;
#[path = "metadata_validation_test.rs"]
mod metadata_validation_test;
#[path = "migrate_job_test.rs"]
mod migrate_job_test;
#[path = "mismatch_boot_test.rs"]
mod mismatch_boot_test;
#[path = "multipart_complete_resilience_test.rs"]
mod multipart_complete_resilience_test;
#[path = "multipart_etag_test.rs"]
mod multipart_etag_test;
#[path = "optimization_test.rs"]
mod optimization_test;
#[path = "parity_test.rs"]
mod parity_test;
#[path = "public_prefix_list_test.rs"]
mod public_prefix_list_test;
#[path = "public_prefix_test.rs"]
mod public_prefix_test;
#[path = "quota_test.rs"]
mod quota_test;
#[path = "reference_lock_race_test.rs"]
mod reference_lock_race_test;
#[path = "replication_target_only_test.rs"]
mod replication_target_only_test;
#[path = "replication_test.rs"]
mod replication_test;
#[path = "s3_api_test.rs"]
mod s3_api_test;
#[path = "s3_backend_test.rs"]
mod s3_backend_test;
#[path = "s3_compat_test.rs"]
mod s3_compat_test;
#[path = "s3_correctness_test.rs"]
mod s3_correctness_test;
#[path = "s3_integration_test.rs"]
mod s3_integration_test;
#[path = "savings_test.rs"]
mod savings_test;
#[path = "spawn_cwd_guard_test.rs"]
mod spawn_cwd_guard_test;
#[path = "storage_resilience_test.rs"]
mod storage_resilience_test;
#[path = "streaming_copy_test.rs"]
mod streaming_copy_test;
#[path = "unmanaged_objects_test.rs"]
mod unmanaged_objects_test;
