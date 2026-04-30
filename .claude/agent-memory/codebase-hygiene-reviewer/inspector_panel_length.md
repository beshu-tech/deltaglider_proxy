---
name: InspectorPanel component length
description: InspectorPanel.tsx is 800 LOC mixing ShareDurationButton, download/share modal, storage stats, and metadata display — candidate for extraction in a focused refactoring pass
type: project
---

InspectorPanel.tsx is ~800 LOC and mixes 4+ concerns:
- ShareDurationButton sub-component (L74-194, ~120 LOC)
- Download/share modal state + rendering (L654-796, ~130 LOC)
- Storage stats with bucket policy fetch (L488-584, ~85 LOC)
- Metadata sections and header

**Why:** Any change requires holding 800 lines of mental model. The modal and ShareDurationButton are self-contained enough to extract.

**How to apply:** When modifying InspectorPanel, consider extracting ShareDurationButton to its own file and the download/share modal to a DownloadShareModal component. Defer until there's a concrete reason to touch the file.
