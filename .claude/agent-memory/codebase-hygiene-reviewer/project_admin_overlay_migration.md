---
name: Admin UI migration state
description: AdminPage replaced AdminOverlay; SettingsPage is embedded within AdminPage for config tabs; bucket policies now managed solely in BackendsPanel
type: project
---

AdminPage.tsx is the full-screen admin UI with sidebar tabs (users, groups, auth, metrics, backends/storage, backend/connection, limits, security, logging). It embeds SettingsPage for config tabs via `embeddedTab` prop, and uses UsersPanel+UserForm for the Users tab, GroupsPanel for the Groups tab, AuthenticationPanel for the Auth tab, BackendsPanel for the Storage tab, and MetricsPage embedded for metrics.

SettingsPage (~500 lines) is NOT dead code -- it renders the config form when embedded by AdminPage. The standalone rendering path was removed; `embeddedTab` is always provided.

Bucket policy management is exclusively in BackendsPanel. SettingsPage no longer touches bucket_policies (ghost state was removed 2026-04-06).

**Why:** Knowing the component hierarchy prevents accidentally deleting SettingsPage as dead code.
**How to apply:** SettingsPage handles backend, limits, security, and logging tabs. BackendsPanel handles storage/compression and bucket policies. Do not add bucket_policies state back to SettingsPage.
