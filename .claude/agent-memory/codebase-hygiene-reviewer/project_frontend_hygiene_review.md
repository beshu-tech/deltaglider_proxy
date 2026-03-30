---
name: Frontend hygiene review findings
description: React/TypeScript admin UI code quality review — implemented fixes and remaining structural observations (March 2026)
type: project
---

## Implemented Fixes (2026-03-28)

1. **Consolidated byte formatters**: Deleted `fmtBytes` (MetricsPage) and `formatSize` (FilePreview), replaced with canonical `formatBytes` from utils.ts. Added TB support to `formatBytes`.

2. **Moved InspectorPanel inner components to module level**: `Section` (renamed `InspectorSection`) and `InfoRow` were defined inside the render function body, causing React to remount their DOM subtrees on every state change. Moved to module-level standalone components using `useColors()`.

3. **Extracted CredentialsBanner component**: Deduplicated identical 15-line credential display Alert from UsersPanel.tsx and UserForm.tsx into a shared `CredentialsBanner.tsx` component.

## Remaining Findings (not implemented — medium effort)

- **UsersPanel/GroupsPanel structural duplication**: The master-detail panel layout (search, list with hover, detail form) is copy-pasted between these two 265/415-line files. ~100 lines of identical layout/style/state logic. Worth extracting a `MasterDetailPanel` with render props if these panels gain more features.

- **MetricsPage.tsx (560 lines)**: Contains a Prometheus text parser, metric access helpers, formatters, 4 sub-components, and the main dashboard. The parser is a pure function with zero React dependencies — extracting it to `utils/prometheus.ts` would make it testable and discoverable.

- **ConnectPage label duplication**: The label style `{ fontSize: 12, fontWeight: 600, color: TEXT_MUTED, ... }` appears 4 times inline. `shared-styles.ts` already exports `labelStyle` via `useCardStyles()` but ConnectPage doesn't use it.

- **Sidebar hover handlers**: `onMouseEnter`/`onMouseLeave` handlers that set `e.currentTarget.style.color` are copy-pasted 3 times in Sidebar.tsx. Could use CSS `:hover` or a small wrapper component.

**Why:** These observations help prioritize future cleanup work.
**How to apply:** Reference when touching UsersPanel, GroupsPanel, MetricsPage, ConnectPage, or Sidebar.
