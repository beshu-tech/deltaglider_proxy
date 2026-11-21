// SPDX-License-Identifier: BUSL-1.1

//! The deltaspaces of a filesystem bucket that hold a reference, walked one
//! directory entry at a time. The memory of a walk is the stack of open
//! directories (one per level of the path), never the list of the deltaspaces:
//! a bucket with a folder for each object has millions of them.

use super::*;
use futures::stream::BoxStream;
use futures::StreamExt;

/// The state of the walk: the directories that it is in, from the scope down.
struct Walk {
    base: PathBuf,
    names: Names,
    stack: Vec<(PathBuf, fs::ReadDir)>,
}

impl Walk {
    /// The next deltaspace with a reference, relative to `base`; `None` at
    /// the end. A directory that is gone (a concurrent delete) holds none.
    async fn next(&mut self) -> Result<Option<String>, StorageError> {
        loop {
            let Some((dir, entries)) = self.stack.last_mut() else {
                return Ok(None);
            };
            let Some(entry) = entries.next_entry().await? else {
                self.stack.pop();
                continue;
            };
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| is_internal_dir_name(n, self.names))
            {
                continue;
            }
            if entry.file_type().await?.is_dir() {
                let path = entry.path();
                match fs::read_dir(&path).await {
                    Ok(entries) => self.stack.push((path, entries)),
                    Err(e) if fsio::absent(&e) => {}
                    Err(e) => return Err(io_to_storage_error(e)),
                }
            } else if entry.file_name() == crate::types::REFERENCE_FILE_NAME {
                if let Ok(relative) = dir.strip_prefix(&self.base) {
                    return Ok(Some(relative.to_string_lossy().into_owned()));
                }
            }
        }
    }
}

/// [`StorageBackend::list_reference_prefix_stream`] of the filesystem backend.
pub(super) async fn stream(
    backend: &FilesystemBackend,
    bucket: &str,
    scope: &str,
) -> Result<BoxStream<'static, Result<String, StorageError>>, StorageError> {
    // A `.`, `..` or empty segment names no directory of this backend.
    if check_path_segments(scope, "", backend.case_insensitive).is_err() {
        return Ok(futures::stream::empty().boxed());
    }
    let base = backend.bucket_dir(bucket).join("deltaspaces");
    let scope_dir = backend.deltaspace_dir(bucket, scope)?;
    // A stat error is an error (B11), never "no references".
    if !fsio::dir_exists(&scope_dir).await? {
        return Ok(futures::stream::empty().boxed());
    }
    let first = match fs::read_dir(&scope_dir).await {
        Ok(entries) => vec![(scope_dir, entries)],
        Err(e) if fsio::absent(&e) => Vec::new(),
        Err(e) => return Err(io_to_storage_error(e)),
    };
    let walk = Walk {
        base,
        names: backend.names(),
        stack: first,
    };
    Ok(futures::stream::try_unfold(walk, |mut walk| async move {
        Ok(walk.next().await?.map(|name| (name, walk)))
    })
    .boxed())
}
