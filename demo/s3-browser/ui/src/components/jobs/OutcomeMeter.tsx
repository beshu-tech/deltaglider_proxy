import { deriveMeter, type OutcomeMeterInput } from '../../jobsView';
import './OutcomeMeter.css';

/** A run's outcome as a calm-by-default meter: [proportional track] [label].
 *  No status dot: the status tag beside it already carries the colour. All the decision logic is the pure `deriveMeter` (in jobsView, with
 *  the other job-display helpers + its regression test); this component only
 *  maps that view to DOM (colours: the palette vars in OutcomeMeter.css). */
export default function OutcomeMeter({ bar = true, ...props }: OutcomeMeterInput & {
  /** The proportional track. Off: only the words (a settled
   *  run in a list, where a full bar for one object says nothing). */
  bar?: boolean;
}) {
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
          '--m-green-pct': `${m.greenPct}%`,
          '--m-red-pct': `${m.redPct}%`,
        } as React.CSSProperties
      }
    >
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
