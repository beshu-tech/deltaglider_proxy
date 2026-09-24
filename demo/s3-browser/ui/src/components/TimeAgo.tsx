import { relativeTime } from '../utils';

/** Relative time ("3h ago") with the full timestamp on hover (native title —
 *  AntD tooltips are disabled globally). Unix SECONDS in; "—" for null. */
export default function TimeAgo({ ts }: { ts?: number | null }) {
  if (!ts) return <span>—</span>;
  const date = new Date(ts * 1000);
  return <span title={date.toLocaleString()}>{relativeTime(date)}</span>;
}
