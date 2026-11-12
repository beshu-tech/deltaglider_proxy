// SPDX-License-Identifier: BUSL-1.1

//! What this process knows about its coordination bucket, as one shared
//! value: `/_/ready` and the S3 authentication gate read it, the boot probe,
//! the background re-probe and the config sync write it.
//!
//! Two facts:
//! - is the bucket PROVEN to enforce conditional writes (the shared
//!   [`ProfileCell`]);
//! - did this process CONFIRM its IAM database against the bucket (one pull
//!   or one upload that ended well). Until then the local database is the
//!   copy of an earlier run: a user that a peer removed since is still in it.
//!
//! The decisions are pure: [`readiness_word`] and [`stale_iam_service`].

use super::if_match::ProfileCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Inner {
    profile: ProfileCell,
    iam_confirmed: AtomicBool,
    /// `DGP_IAM_SERVE_UNCONFIRMED`: the operator accepts the local copy.
    serve_unconfirmed: bool,
    /// The live IAM state, once the boot has one: readiness asks it where
    /// the credentials of the S3 API come from.
    iam: std::sync::OnceLock<crate::iam::SharedIamState>,
}

/// The env var that ends the refusal of an unconfirmed IAM database.
pub const SERVE_UNCONFIRMED_ENV: &str = "DGP_IAM_SERVE_UNCONFIRMED";

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("iam_confirmed", &self.iam_confirmed)
            .field("serve_unconfirmed", &self.serve_unconfirmed)
            .finish_non_exhaustive()
    }
}

/// The coordination state of this process. `Default` = no coordination
/// bucket (single instance). Cheap to clone; every clone shares one state.
#[derive(Debug, Clone, Default)]
pub struct CoordinationStatus(Option<Arc<Inner>>);

impl CoordinationStatus {
    /// A process with a coordination bucket whose verdict lives in `profile`.
    pub fn with_bucket(profile: ProfileCell, serve_unconfirmed: bool) -> Self {
        Self(Some(Arc::new(Inner {
            profile,
            iam_confirmed: AtomicBool::new(false),
            serve_unconfirmed,
            iam: std::sync::OnceLock::new(),
        })))
    }

    /// Is the bucket proven? `None`: no coordination bucket.
    pub fn bucket_proven(&self) -> Option<bool> {
        self.0.as_ref().map(|i| i.profile.get().is_some())
    }

    /// The local IAM database is confirmed against the bucket (a pull or an
    /// upload ended well), or does not come from it (declarative IAM).
    pub fn confirm_iam(&self) {
        if let Some(i) = &self.0 {
            i.iam_confirmed.store(true, Ordering::SeqCst);
        }
    }

    /// Give the status the live IAM state (once, at boot).
    pub fn attach_iam(&self, iam: crate::iam::SharedIamState) {
        if let Some(i) = &self.0 {
            let _ = i.iam.set(iam);
        }
    }

    /// Would the S3 API refuse the users of the IAM database now? True only
    /// when the rule ([`stale_iam_service`]) refuses: the credentials come
    /// from the IAM database AND no sync confirmed it AND the operator did
    /// not accept the local copy. Without an attached IAM state the source
    /// is taken as the IAM database.
    pub fn refuses_iam_users(&self) -> bool {
        let source = match self.0.as_ref().and_then(|i| i.iam.get()) {
            Some(iam) => credential_source(&iam.load()),
            None => CredentialSource::IamDatabase,
        };
        stale_iam_service(self.iam_sync(), source) == StaleIamService::Refuse
    }

    /// The sync state of the IAM database.
    pub fn iam_sync(&self) -> IamSync {
        match &self.0 {
            None => IamSync::NotConfigured,
            Some(i) if i.iam_confirmed.load(Ordering::SeqCst) => IamSync::Confirmed,
            Some(i) if i.serve_unconfirmed => IamSync::Overridden,
            Some(_) => IamSync::Unconfirmed,
        }
    }
}

/// The word of `/_/ready` for a fact that is `None` without a coordination
/// bucket (pure).
pub fn readiness_word(fact: Option<bool>, yes: &'static str, no: &'static str) -> &'static str {
    match fact {
        None => "absent",
        Some(true) => yes,
        Some(false) => no,
    }
}

/// Is the local IAM database confirmed against the coordination bucket?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IamSync {
    /// No coordination bucket: the local database is the only copy.
    NotConfigured,
    /// This process pulled or uploaded with success at least once.
    Confirmed,
    /// Not yet: the local database is what an earlier run left.
    Unconfirmed,
    /// Not yet, and the operator set [`SERVE_UNCONFIRMED_ENV`]: the node
    /// serves the local copy. A good sync still moves it to `Confirmed`.
    Overridden,
}

/// Where the credentials of the S3 API come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Open access: no credentials.
    Open,
    /// The bootstrap pair of the config file or the environment.
    Bootstrap,
    /// IAM users (the config database, which the sync bucket shares).
    IamDatabase,
}

/// What the S3 API does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleIamService {
    Serve,
    /// `503`, retryable: the node cannot tell whether its users are current.
    Refuse,
}

/// THE rule for a database that may be stale (pure). A node refuses only
/// when its users come from the IAM database AND a coordination bucket is
/// configured AND this process did not confirm the database against it yet.
/// Open access and the bootstrap pair do not come from the shared database,
/// so they are served. The node is not ready while it refuses
/// (`NodeConditions::iam_sync_unconfirmed`).
pub fn stale_iam_service(sync: IamSync, source: CredentialSource) -> StaleIamService {
    match (sync, source) {
        (IamSync::Unconfirmed, CredentialSource::IamDatabase) => StaleIamService::Refuse,
        _ => StaleIamService::Serve,
    }
}

/// The IAM database is not confirmed against the coordination bucket: no
/// credential or principal may be judged against it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IamUnconfirmed;

/// Where the credentials of a request come from, by IAM state (pure).
pub fn credential_source(iam: &crate::iam::IamState) -> CredentialSource {
    match iam {
        crate::iam::IamState::Disabled => CredentialSource::Open,
        crate::iam::IamState::Legacy(_) => CredentialSource::Bootstrap,
        crate::iam::IamState::Iam(_) => CredentialSource::IamDatabase,
    }
}

impl CoordinationStatus {
    /// THE gate on the IAM index. The IAM state is the one a credential or a
    /// principal (a key pair, a login-as, a browser-connect, an OAuth user,
    /// an admin session of an IAM user) may be judged against: the state
    /// itself, or `Err` while [`stale_iam_service`] refuses. Code that
    /// reads the users of an `IamState::Iam` to judge somebody takes it
    /// from here (source guard
    /// `every_credential_read_of_the_iam_index_goes_through_the_gate`).
    /// Requests that read no IAM database (anonymous, public prefix, OPTIONS,
    /// `HEAD /`) never call it.
    pub fn credential_view<'a>(
        &self,
        iam: &'a crate::iam::IamState,
    ) -> Result<&'a crate::iam::IamState, IamUnconfirmed> {
        match stale_iam_service(self.iam_sync(), credential_source(iam)) {
            StaleIamService::Serve => Ok(iam),
            StaleIamService::Refuse => Err(IamUnconfirmed),
        }
    }
}

/// [`CoordinationStatus::credential_view`] for a router that may carry no
/// status: that case fails CLOSED for IAM users, as a missing IAM extension
/// denies all.
pub fn credential_view_or_closed<'a>(
    status: Option<&CoordinationStatus>,
    iam: &'a crate::iam::IamState,
) -> Result<&'a crate::iam::IamState, IamUnconfirmed> {
    match status {
        Some(s) => s.credential_view(iam),
        None if credential_source(iam) == CredentialSource::IamDatabase => Err(IamUnconfirmed),
        None => Ok(iam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unconfirmed_database_users_are_refused() {
        use CredentialSource::*;
        use IamSync::*;
        use StaleIamService::*;
        let table = [
            (NotConfigured, Open, Serve),
            (NotConfigured, Bootstrap, Serve),
            (NotConfigured, IamDatabase, Serve),
            (Confirmed, Open, Serve),
            (Confirmed, Bootstrap, Serve),
            (Confirmed, IamDatabase, Serve),
            (Unconfirmed, Open, Serve),
            (Unconfirmed, Bootstrap, Serve),
            (Unconfirmed, IamDatabase, Refuse),
            (Overridden, Open, Serve),
            (Overridden, Bootstrap, Serve),
            (Overridden, IamDatabase, Serve),
        ];
        for (sync, source, want) in table {
            assert_eq!(stale_iam_service(sync, source), want, "{sync:?} {source:?}");
        }
    }

    #[test]
    fn the_status_follows_the_cell_and_the_confirmation() {
        let none = CoordinationStatus::default();
        assert_eq!(none.bucket_proven(), None);
        none.confirm_iam();
        assert_eq!(none.iam_sync(), IamSync::NotConfigured);

        let cell = ProfileCell::unknown();
        let status = CoordinationStatus::with_bucket(cell.clone(), false);
        let seen_elsewhere = status.clone();
        assert_eq!(status.bucket_proven(), Some(false));
        assert_eq!(status.iam_sync(), IamSync::Unconfirmed);
        cell.prove(Default::default());
        status.confirm_iam();
        assert_eq!(seen_elsewhere.bucket_proven(), Some(true));
        assert_eq!(seen_elsewhere.iam_sync(), IamSync::Confirmed);

        // The override serves the local copy; a good sync still confirms.
        let over = CoordinationStatus::with_bucket(ProfileCell::unknown(), true);
        assert_eq!(over.iam_sync(), IamSync::Overridden);
        over.confirm_iam();
        assert_eq!(over.iam_sync(), IamSync::Confirmed);
    }

    #[test]
    fn the_credential_view_follows_the_rule() {
        use crate::iam::{IamIndex, IamState};
        let iam = IamState::Iam(IamIndex::from_users(vec![]));
        let open = IamState::Disabled;
        let unconfirmed = CoordinationStatus::with_bucket(ProfileCell::unknown(), false);
        assert_eq!(
            unconfirmed.credential_view(&iam).err(),
            Some(IamUnconfirmed)
        );
        assert!(unconfirmed.credential_view(&open).is_ok());
        let over = CoordinationStatus::with_bucket(ProfileCell::unknown(), true);
        assert!(over.credential_view(&iam).is_ok());
        unconfirmed.confirm_iam();
        assert!(unconfirmed.credential_view(&iam).is_ok());
        assert!(CoordinationStatus::default().credential_view(&iam).is_ok());
        // No status at all: IAM users fail closed, the rest is served.
        assert!(credential_view_or_closed(None, &iam).is_err());
        assert!(credential_view_or_closed(None, &open).is_ok());
    }

    #[test]
    fn only_a_node_that_refuses_users_is_not_ready_for_them() {
        use crate::iam::{IamIndex, IamState};
        use arc_swap::ArcSwap;
        let attach = |s: &CoordinationStatus, iam: IamState| {
            s.attach_iam(Arc::new(ArcSwap::from_pointee(iam)));
        };
        let unconfirmed = CoordinationStatus::with_bucket(ProfileCell::unknown(), false);
        attach(&unconfirmed, IamState::Iam(IamIndex::from_users(vec![])));
        assert!(unconfirmed.refuses_iam_users());
        // The credentials come from elsewhere: nothing to refuse.
        let open = CoordinationStatus::with_bucket(ProfileCell::unknown(), false);
        attach(&open, IamState::Disabled);
        assert!(!open.refuses_iam_users());
        let over = CoordinationStatus::with_bucket(ProfileCell::unknown(), true);
        attach(&over, IamState::Iam(IamIndex::from_users(vec![])));
        assert!(!over.refuses_iam_users());
        unconfirmed.confirm_iam();
        assert!(!unconfirmed.refuses_iam_users());
        assert!(!CoordinationStatus::default().refuses_iam_users());
    }

    /// THE guard of the IAM index: every function outside `src/iam/` that
    /// reads the users of an `IamState::Iam` (a key pair, a login, a
    /// principal, an admin session of an IAM user) takes them from
    /// [`CoordinationStatus::credential_view`], so a new authentication
    /// path cannot skip the rule of an unconfirmed database. The exempt
    /// functions judge nobody; each has its reason.
    #[test]
    fn every_credential_read_of_the_iam_index_goes_through_the_gate() {
        // (file, function, why it needs no gate)
        const EXEMPT: &[(&str, &str, &str)] = &[
            (
                "src/api/admin/auth.rs",
                "whoami_mode",
                "a label of the mode, no user",
            ),
            (
                "src/api/admin/auth.rs",
                "non_admin_bulk_session",
                "its caller passes the gated view",
            ),
            (
                "src/api/admin/auth.rs",
                "session_principal_is_admin",
                "its callers pass the gated view",
            ),
            (
                "src/api/admin/auth.rs",
                "session_actor_label",
                "an audit label of an admitted session",
            ),
            (
                "src/api/admin/auth.rs",
                "bucket_view_session",
                "reads the view that the gate returned",
            ),
            (
                "src/api/admin/objects.rs",
                "for_user",
                "its callers pass the gated view",
            ),
            (
                "src/api/admin/config/mod.rs",
                "remove_bootstrap_credentials",
                "a warning text, no login",
            ),
            (
                "src/api/auth.rs",
                "classify_auth_gate",
                "picks the path; the Iam path calls the gate before it reads a user",
            ),
            (
                "src/api/admin/users.rs",
                "",
                "the admin edits users; it judges nobody",
            ),
            (
                "src/api/admin/users.rs",
                "when_empty",
                "the empty-IAM outcome, no user",
            ),
            ("src/startup.rs", "", "boot state, judges nobody"),
            (
                "src/config_db_sync.rs",
                "",
                "the sync rebuilds the index, judges nobody",
            ),
        ];
        let mut offenders = Vec::new();
        let mut gated = 0;
        for (rel, text) in crate::source_scan::prod_sources("src") {
            if rel.starts_with("src/iam/") || rel == "src/coordination/status.rs" {
                continue;
            }
            let prod = crate::source_scan::prod_mask(&text);
            for (at, line) in text.lines().enumerate() {
                if !prod[at]
                    || line.trim_start().starts_with("//")
                    || !line.contains("IamState::Iam(")
                {
                    continue;
                }
                let (name, body) = crate::source_scan::enclosing_fn(&text, at);
                if body.contains("credential_view") {
                    gated += 1;
                    continue;
                }
                let exempt = EXEMPT
                    .iter()
                    .any(|(f, n, _)| *f == rel && (n.is_empty() || *n == name));
                if !exempt {
                    offenders.push(format!("{rel}:{} in fn {name}", at + 1));
                }
            }
        }
        assert!(gated >= 5, "the scan found the gated reads ({gated})");
        assert!(
            offenders.is_empty(),
            "these functions read the users of the IAM index without credential_view:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn the_readiness_words() {
        assert_eq!(readiness_word(None, "proven", "unproven"), "absent");
        assert_eq!(readiness_word(Some(true), "proven", "unproven"), "proven");
        assert_eq!(
            readiness_word(Some(false), "proven", "unproven"),
            "unproven"
        );
    }
}
