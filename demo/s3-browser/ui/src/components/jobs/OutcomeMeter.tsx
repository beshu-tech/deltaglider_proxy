import { useColors } from '../../ThemeContext';
import { deriveMeter, type OutcomeMeterInput } from '../../jobsView';
import './OutcomeMeter.css';

/** A run's outcome as a calm-by-default meter: [dot] [proportional track]
 *  [label]. All the decision logic is the pure `deriveMeter` (in jobsView, with
 *  the other job-display helpers + its regression test); this component only
 *  maps that view to DOM + injects theme colors as CSS custom properties. */
export default function OutcomeMeter({ bar = true, ...props }: OutcomeMeterInput & {
  /** The proportional track. Off: only the dot and the words (a settled
   *  run in a list, where a full bar for one object says nothing). */
  bar?: boolean;
}) {
  const c = useColors();
  const m = deriveMeter(props);
  const indeterminate = (m.state === 'running' && props.percent == null) || m.state === 'cancelling';
  return (
    <div
      className="dg-meter"
      data-state={m.state}
      data-bar={bar ? undefined : 'off'}
      title={m.aria}
      role="img"
      aria-label={m.aria}
      style={
        {
          '--m-green': c.ACCENT_GREEN,
          '--m-red': c.ACCENT_RED,
          '--m-amber': c.ACCENT_AMBER,
          '--m-track': c.BORDER,
          '--m-text-muted': c.TEXT_MUTED,
          '--m-text': c.TEXT_PRIMARY,
          '--m-green-pct': `${m.greenPct}%`,
          '--m-red-pct': `${m.redPct}%`,
        } as React.CSSProperties
      }
    >
      <span className="dg-meter-dot" data-dot={m.dot} aria-hidden="true" />
      {bar && (
        <span className="dg-meter-track" aria-hidden="true">
          {m.greenPct > 0 && <span className="dg-meter-seg dg-meter-green" />}
          {m.redPct > 0 && <span className="dg-meter-seg dg-meter-red" />}
          {indeterminate && <span className="dg-meter-indeterminate" />}
        </span>
      )}
      <span className="dg-meter-label">{m.label}</span>
    </div>
  );
}
