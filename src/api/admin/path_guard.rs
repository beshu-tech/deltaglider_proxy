// SPDX-License-Identifier: BUSL-1.1

//! Validated bucket / object-path inputs for admin handlers (S6).
//!
//! Admin handlers call the engine directly, so s3s never checks their bucket
//! names. On the filesystem backend `root.join("/etc")` is `/etc` and
//! `root.join("../x")` leaves the data root. These newtypes validate at
//! deserialization: a request that names a bad bucket or path never reaches
//! a handler body. `admin_inputs_use_validated_types` (below) scans the admin
//! request structs so a new `bucket: String` field cannot slip in.

use serde::Deserialize;

/// Bucket name that passes the S3 naming rules (`security::validate_bucket_name`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct AdminBucket(String);

/// Object key or prefix with no path-escape shape (see [`check_object_path`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(try_from = "String")]
pub struct AdminObjectPath(String);

/// The bucket-name rule for admin inputs.
pub fn check_bucket(name: &str) -> Result<(), String> {
    crate::security::validate_bucket_name(name).map_err(|e| format!("invalid bucket name: {e}"))
}

/// The key/prefix rule for admin inputs: no leading `/` (absolute path), no
/// `.` or `..` segment (directory escape), no NUL. Empty is allowed (whole
/// bucket). `a//b` stays allowed: legacy S3 objects with empty segments must
/// stay reachable for cleanup; the filesystem backend refuses them itself.
pub fn check_object_path(p: &str) -> Result<(), String> {
    if p.starts_with('/') {
        return Err(format!("invalid path {p:?}: must not start with '/'"));
    }
    if p.contains('\0') {
        return Err("invalid path: contains NUL".into());
    }
    if p.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(format!(
            "invalid path {p:?}: '.' and '..' segments are not allowed"
        ));
    }
    Ok(())
}

impl TryFrom<String> for AdminBucket {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        check_bucket(&s)?;
        Ok(Self(s))
    }
}

impl TryFrom<String> for AdminObjectPath {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        check_object_path(&s)?;
        Ok(Self(s))
    }
}

macro_rules! str_newtype {
    ($t:ty) => {
        impl std::ops::Deref for $t {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }
        impl AsRef<str> for $t {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl serde::Serialize for $t {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }
        impl $t {
            pub fn into_string(self) -> String {
                self.0
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}
str_newtype!(AdminBucket);
str_newtype!(AdminObjectPath);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_rule_refuses_path_shapes() {
        for bad in ["/etc", "../x", "..", ".", "a/b", "", "Upper", "a\0b"] {
            assert!(check_bucket(bad).is_err(), "{bad:?} must be refused");
        }
        for ok in ["releases", "db-archive", "a.b.c"] {
            assert!(check_bucket(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn object_path_rule_refuses_escapes() {
        for bad in [
            "/abs/other",
            "../../etc",
            "a/../b",
            "a/./b",
            "..",
            ".",
            "a\0b",
        ] {
            assert!(check_object_path(bad).is_err(), "{bad:?} must be refused");
        }
        for ok in ["", "a", "a/b/", "a//b", "..a/b..", "v1.0/x.tar"] {
            assert!(check_object_path(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn deserialize_rejects_bad_values() {
        assert!(serde_json::from_str::<AdminBucket>(r#""/etc""#).is_err());
        assert!(serde_json::from_str::<AdminObjectPath>(r#""../x""#).is_err());
        let b: AdminBucket = serde_json::from_str(r#""releases""#).unwrap();
        assert_eq!(&*b, "releases");
    }

    /// Source guard: every inbound admin request struct (derives Deserialize
    /// but not Serialize) that carries a bucket / prefix / key must use the
    /// validated types. Plain `String` there is the S6 defect.
    #[test]
    fn admin_inputs_use_validated_types() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api/admin");
        // Fields that are not storage paths (UI hints, synthetic trace input).
        const ALLOW: &[&str] = &[
            "auth.rs:BrowserSessionConnectRequest.bucket",
            "auth.rs:OpenBrowserConnectRequest.bucket",
            // Parsed from one comma list, each entry checked in download_zip.
            "objects.rs:ZipQuery.keys",
        ];
        let mut offenders = Vec::new();
        let mut files = vec![];
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                for e2 in std::fs::read_dir(&p).unwrap().flatten() {
                    files.push(e2.path());
                }
            } else {
                files.push(p);
            }
        }
        for path in files
            .iter()
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        {
            let src = std::fs::read_to_string(path).unwrap();
            let fname = path.file_name().unwrap().to_string_lossy().to_string();
            let lines: Vec<&str> = src.lines().collect();
            for (n, l) in src.lines().enumerate() {
                // A raw `Path<String>` bucket is fine only when the handler
                // runs `check_bucket` on it at once (`start_migrate` trims
                // first, which AdminBucket would refuse).
                let checked_at_once = lines[n..(n + 16).min(lines.len())]
                    .iter()
                    .any(|l| l.contains("check_bucket("));
                if l.contains(concat!("Path(bucket): ", "Path<String>"))
                    && fname != "path_guard.rs"
                    && !checked_at_once
                {
                    offenders.push(format!("{fname}:{}: Path<String> bucket", n + 1));
                }
            }
            let mut i = 0;
            while i < lines.len() {
                let l = lines[i].trim();
                let inbound = l.starts_with("#[derive(")
                    && l.contains("Deserialize")
                    && !l.contains("Serialize,")
                    && !l.contains(" Serialize)")
                    && !l.contains("(Serialize");
                if !inbound {
                    i += 1;
                    continue;
                }
                // Find the struct line, then scan fields to the closing brace.
                let mut j = i + 1;
                while j < lines.len()
                    && !lines[j].contains("struct ")
                    && !lines[j].contains("enum ")
                {
                    j += 1;
                }
                if j >= lines.len() || lines[j].contains("enum ") {
                    i = j;
                    continue;
                }
                let name = lines[j]
                    .split("struct ")
                    .nth(1)
                    .unwrap_or("")
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let mut k = j + 1;
                while k < lines.len() && !lines[k].starts_with('}') {
                    let f = lines[k].trim().trim_start_matches("pub ");
                    if let Some((field, ty)) = f.split_once(':') {
                        let field = field.trim();
                        let pathy = field == "bucket"
                            || field == "buckets"
                            || field.ends_with("_bucket")
                            || field.ends_with("prefix")
                            || field == "key"
                            || (field.ends_with("_key") && !field.contains("access_key"))
                            || field == "relative"
                            || field == "keys";
                        if pathy
                            && ty.contains("String")
                            && !ALLOW.contains(&format!("{fname}:{name}.{field}").as_str())
                        {
                            offenders.push(format!("{fname}:{name}.{field}"));
                        }
                    }
                    k += 1;
                }
                i = k;
            }
        }
        assert!(
            offenders.is_empty(),
            "admin request fields must use AdminBucket / AdminObjectPath, found String: {offenders:?}"
        );
    }
}
