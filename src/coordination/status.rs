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

#[derive(Debug)]
struct Inner {
    profile: ProfileCell,
    iam_confirmed: AtomicBool,
    /// `DGP_IAM_SERVE_UNCONFIRMED`: the operator accepts the local copy.
    serve_unconfirmed: bool,
}

/// The env var that ends the refusal of an unconfirmed IAM database.
pub const SERVE_UNCONFIRMED_ENV: &str = "DGP_IAM_SERVE_UNCONFIRMED";

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
    fn the_readiness_words() {
        assert_eq!(readiness_word(None, "proven", "unproven"), "absent");
        assert_eq!(readiness_word(Some(true), "proven", "unproven"), "proven");
        assert_eq!(
            readiness_word(Some(false), "proven", "unproven"),
            "unproven"
        );
    }
}
