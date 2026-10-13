import { relativeTime } from '../utils';
import { tickPeriodMs, useNow } from '../hooks/useNow';

/** Relative time ("3h ago") with the full timestamp on hover (native title —
 *  AntD tooltips are disabled globally). Unix SECONDS in; "—" for null. The
 *  text moves between polls ("12s ago" → "13s ago"). */
export default function TimeAgo({ ts }: { ts?: number | null }) {
  const now = useNow(tickPeriodMs(ts ? Date.now() / 1000 - ts : 3600));
  if (!ts) return <span>—</span>;
  const date = new Date(ts * 1000);
  return <span title={date.toLocaleString()}>{relativeTime(date, { now })}</span>;
}
