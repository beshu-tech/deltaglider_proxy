// SPDX-License-Identifier: BUSL-1.1

//! Progress that a job row has no column for: the objects that a migrate's
//! verify phase checked so far. In process memory only (the jobs DB and the
//! worker are node-local, so the node that serves `GET /jobs` runs the
//! job); a restart loses it, and the verify percent starts again from its
//! phase floor.

use std::collections::HashMap;
use std::sync::LazyLock;

use super::store::MaintenanceJob;

/// (job id, created_at): ids restart at 1 in every in-memory test DB.
type JobKey = (i64, i64);

static VERIFIED: LazyLock<parking_lot::Mutex<HashMap<JobKey, i64>>> =
    LazyLock::new(Default::default);

fn key(job: &MaintenanceJob) -> JobKey {
    (job.id, job.created_at)
}

/// The verify phase of `job` checked `n` objects so far.
pub(crate) fn set_verified(job: &MaintenanceJob, n: i64) {
    VERIFIED.lock().insert(key(job), n);
}

/// The objects that the verify phase of `job` checked, if it runs here.
pub fn verified(job: &MaintenanceJob) -> Option<i64> {
    VERIFIED.lock().get(&key(job)).copied()
}

/// Forget `job` (its verify phase ended).
pub(crate) fn clear(job: &MaintenanceJob) {
    VERIFIED.lock().remove(&key(job));
}
