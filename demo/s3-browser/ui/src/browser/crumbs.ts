/**
 * Pure: the crumbs after the bucket. With `collapseMiddle`, the folders
 * between the bucket and the current folder become ONE "…" link to the
 * parent folder, so the current folder keeps its room.
 */
export function crumbSegments(
  all: { label: string; prefix: string }[],
  collapseMiddle: boolean,
): { label: string; prefix: string; collapsed?: boolean }[] {
  if (!collapseMiddle || all.length < 2) return all;
  return [{ ...all[all.length - 2], label: '…', collapsed: true }, all[all.length - 1]];
}
