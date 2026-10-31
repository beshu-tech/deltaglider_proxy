/**
 * UX harvest D42: the latency histogram read "5ms 10ms 5ms 10ms 25ms …"
 * because the per-operation series were concatenated, and the average
 * latency came from the first series only.
 */
import { describe, expect, test } from 'vitest';
import {
  fmtLatencyBound,
  fmtRatioBound,
  histStats,
  histogramBars,
  nonZeroByLabel,
  parsePrometheus,
} from '../components/dashboard/prometheus';

const LATENCY = `# HELP deltaglider_http_request_duration_seconds HTTP request duration in seconds
# TYPE deltaglider_http_request_duration_seconds histogram
deltaglider_http_request_duration_seconds_bucket{method="GET",operation="get_object",le="0.005"} 2
deltaglider_http_request_duration_seconds_bucket{method="GET",operation="get_object",le="0.01"} 3
deltaglider_http_request_duration_seconds_bucket{method="GET",operation="get_object",le="0.025"} 3
deltaglider_http_request_duration_seconds_bucket{method="GET",operation="get_object",le="+Inf"} 3
deltaglider_http_request_duration_seconds_sum{method="GET",operation="get_object"} 0.012
deltaglider_http_request_duration_seconds_count{method="GET",operation="get_object"} 3
deltaglider_http_request_duration_seconds_bucket{method="PUT",operation="put_object",le="0.005"} 0
deltaglider_http_request_duration_seconds_bucket{method="PUT",operation="put_object",le="0.01"} 1
deltaglider_http_request_duration_seconds_bucket{method="PUT",operation="put_object",le="0.025"} 1
deltaglider_http_request_duration_seconds_bucket{method="PUT",operation="put_object",le="+Inf"} 2
deltaglider_http_request_duration_seconds_sum{method="PUT",operation="put_object"} 2.008
deltaglider_http_request_duration_seconds_count{method="PUT",operation="put_object"} 2
`;

describe('histograms over several series', () => {
  const m = parsePrometheus(LATENCY);
  test('one bar per range, labels never repeat', () => {
    const bars = histogramBars(m, 'deltaglider_http_request_duration_seconds', fmtLatencyBound);
    expect(bars).toEqual([
      { range: '<5 ms', count: 2 },
      { range: '5–10 ms', count: 2 },
      { range: '10–25 ms', count: 0 },
      { range: '≥25 ms', count: 1 },
    ]);
  });
  test('the average uses every series', () => {
    const s = histStats(m, 'deltaglider_http_request_duration_seconds');
    expect(s.count).toBe(5);
    expect(s.sum).toBeCloseTo(2.02);
    expect(s.avg).toBeCloseTo(0.404);
  });
  test('an unknown or empty histogram has no bars', () => {
    expect(histogramBars(m, 'nope', fmtLatencyBound)).toEqual([]);
    const zero = parsePrometheus('# HELP x x\n# TYPE x histogram\nx_bucket{le="0.1"} 0\nx_bucket{le="+Inf"} 0\n');
    expect(zero.get('x')?.samples).toHaveLength(2);
    expect(histogramBars(zero, 'x', fmtLatencyBound)).toEqual([]);
  });
});

test('ratio bounds read as percentage ranges', () => {
  const m = parsePrometheus(`# HELP r ratio
# TYPE r histogram
r_bucket{le="0.01"} 1
r_bucket{le="0.05"} 3
r_bucket{le="0.1"} 3
r_bucket{le="+Inf"} 3
r_sum 0.06
r_count 3
`);
  expect(histogramBars(m, 'r', fmtRatioBound)).toEqual([
    { range: '<1%', count: 1 },
    { range: '1–5%', count: 2 },
  ]);
  expect(fmtLatencyBound(2.5)).toBe('2.5 s');
});

test('a counter with only zero values has nothing to chart', () => {
  const m = parsePrometheus(`# HELP d decisions
# TYPE d counter
d{decision="delta"} 0
d{decision="passthrough"} 4
d{decision="reference"} 0
`);
  expect(nonZeroByLabel(m, 'd', 'decision')).toEqual([{ name: 'passthrough', value: 4 }]);
});
