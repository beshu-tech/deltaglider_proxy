---
name: Admin UI migration state
description: AdminPage replaced AdminOverlay; SettingsPage is embedded within AdminPage for config tabs; #/settings route redirects to browser
type: project
---

AdminPage.tsx (formerly AdminOverlay) is the full-screen admin UI with sidebar tabs (connection, backend, proxy, users, groups, metrics, bootstrap). It embeds SettingsPage for config tabs via `embeddedTab` prop, and uses UsersPanel+UserForm for the Users tab, GroupsPanel for the Groups tab, and MetricsPage embedded for metrics.

SettingsPage (600 lines) is NOT dead code -- it renders the config form when embedded by AdminPage. It also has a standalone mode with its own Tabs bar, but that code path is only used if SettingsPage is rendered directly (which no route currently does).

The `#/settings` hash route in App.tsx maps to `'browser'` (a legacy redirect). It does not render SettingsPage standalone anymore.

**Why:** Knowing the component hierarchy prevents accidentally deleting SettingsPage as dead code.
**How to apply:** If removing SettingsPage's standalone Tabs bar (lines ~587+), verify no route renders it without `embeddedTab`.
