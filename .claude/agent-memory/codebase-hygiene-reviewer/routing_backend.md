---
name: Routing backend semantics
description: RoutingBackend resolve vs resolve_existing, listed_bucket_virtual_name returns String not Option
type: project
---

`RoutingBackend` has two resolution paths:
- `resolve(bucket)` — sync, explicit routes only, falls back to default backend. Used for create_bucket.
- `resolve_existing(bucket)` — async, explicit routes first, then HEAD-scans default and other backends. Used for all read/write/delete operations.

`listed_bucket_virtual_name` returns `String` (not `Option<String>`). It was changed from Option after noticing the function was infallible — the `.or_else` always produced Some. The old doc comment claiming "non-default backend buckets NOT exposed without a route" was stale/wrong given `resolve_existing` HEAD-scans all backends.

**Why:** The Option wrapper forced callers into dead `if let Some(...)` branches and the stale doc comment contradicted runtime behavior.

**How to apply:** When modifying listing logic, remember listed_bucket_virtual_name is infallible. All discovered buckets are exposed because resolve_existing will find them at runtime regardless of explicit routes.
