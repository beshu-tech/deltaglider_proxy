---
name: Admin overlay migration state
description: AdminOverlay replaced SettingsPage as primary admin UI; old UsersTab/UserModal deleted; #/settings route still exists as redundant entry point
type: project
---

AdminOverlay.tsx is the new full-screen admin UI with sidebar tabs (connection, backend, proxy, users, security). It embeds SettingsPage for non-Users tabs via `embeddedTab` prop, and uses UsersPanel+UserForm for the Users tab.

The old UsersTab.tsx and UserModal.tsx were deleted as dead code (March 2026). Their Users tab entry was also removed from SettingsPage's standalone Tabs.

**Why:** The old modal-based user management was replaced by the panel-based UserForm inside AdminOverlay. Having both paths was confusing and maintained duplicate code (522 lines).

**How to apply:** The `#/settings` hash route in App.tsx (line ~229) still renders SettingsPage as a standalone page. This is a redundant entry point -- AdminOverlay is the primary admin UI. Consider removing or redirecting this route in a future change.
