import type { ReactNode } from 'react';
import {
  BarElement,
  CategoryScale,
  Chart as ChartJS,
  Legend,
  LinearScale,
  Tooltip,
  type ChartOptions,
} from 'chart.js';
import { Bar } from 'react-chartjs-2';
import {
  BENCHMARK_DOCKER_CPU_PCT,
  BENCHMARK_MODE_LABELS,
  BENCHMARK_SAMPLE_META,
  BENCHMARK_STORAGE_GB,
  BENCHMARK_THROUGHPUT_MBPS,
} from '../data/benchmarkSampleRun';

ChartJS.register(CategoryScale, LinearScale, BarElement, Tooltip, Legend);

const MODE_LABELS = [...BENCHMARK_MODE_LABELS];

function chartCard(title: string, subtitle: string | undefined, children: ReactNode): JSX.Element {
  return (
    <div className="rounded-2xl border border-ink-200 bg-white/90 p-4 shadow-sm dark:border-ink-600 dark:bg-ink-900/80">
      <div className="mb-4 px-1">
        <h3 className="text-base font-extrabold tracking-tight text-ink-900 dark:text-white">{title}</h3>
        {subtitle && (
          <p className="mt-1 text-sm leading-relaxed text-ink-500 dark:text-ink-400">{subtitle}</p>
        )}
      </div>
      {children}
    </div>
  );
}

const axisColor = 'rgba(100, 116, 139, 0.95)';
const gridColor = 'rgba(148, 163, 184, 0.12)';

const baseOptions: ChartOptions<'bar'> = {
  responsive: true,
  maintainAspectRatio: false,
  interaction: { mode: 'index', intersect: false },
  plugins: {
    legend: {
      position: 'bottom',
      labels: {
        color: axisColor,
        boxWidth: 12,
        font: { size: 11, weight: 500 },
      },
    },
    tooltip: {
      backgroundColor: 'rgba(15, 23, 42, 0.94)',
      borderColor: 'rgba(148, 163, 184, 0.35)',
      borderWidth: 1,
      padding: 12,
    },
  },
  scales: {
    x: {
      ticks: { color: axisColor, maxRotation: 0 },
      grid: { color: gridColor },
    },
    y: {
      beginAtZero: true,
      ticks: { color: axisColor },
      grid: { color: gridColor },
    },
  },
};

export default function BenchmarkChartsBody(): JSX.Element {
  const throughputData = {
    labels: MODE_LABELS,
    datasets: [
      {
        label: 'PUT',
        data: [...BENCHMARK_THROUGHPUT_MBPS.put],
        backgroundColor: 'rgba(59, 130, 246, 0.72)',
      },
      {
        label: 'Cold GET',
        data: [...BENCHMARK_THROUGHPUT_MBPS.cold_get],
        backgroundColor: 'rgba(16, 185, 129, 0.72)',
      },
      {
        label: 'Warm GET',
        data: [...BENCHMARK_THROUGHPUT_MBPS.warm_get],
        backgroundColor: 'rgba(251, 146, 60, 0.78)',
      },
    ],
  };

  const storageData = {
    labels: MODE_LABELS,
    datasets: [
      {
        label: 'Logical uploaded (client-visible)',
        data: MODE_LABELS.map(() => BENCHMARK_STORAGE_GB.logical),
        backgroundColor: 'rgba(148, 163, 184, 0.55)',
      },
      {
        label: 'Implied stored (logical − Δ saved)',
        data: [...BENCHMARK_STORAGE_GB.implied_stored],
        backgroundColor: 'rgba(59, 130, 246, 0.68)',
      },
    ],
  };

  const cpuData = {
    labels: MODE_LABELS,
    datasets: [
      {
        label: 'Docker CPU % mean',
        data: [...BENCHMARK_DOCKER_CPU_PCT.mean],
        backgroundColor: 'rgba(250, 204, 21, 0.62)',
      },
      {
        label: 'Docker CPU % max',
        data: [...BENCHMARK_DOCKER_CPU_PCT.max],
        backgroundColor: 'rgba(251, 146, 60, 0.88)',
      },
    ],
  };

  const throughputOpts: ChartOptions<'bar'> = {
    ...baseOptions,
    plugins: {
      ...baseOptions.plugins,
      title: { display: false },
    },
    scales: {
      ...baseOptions.scales,
      y: {
        ...baseOptions.scales?.y,
        title: { display: true, text: 'MB/s', color: axisColor },
      },
    },
  };

  const storageOpts: ChartOptions<'bar'> = {
    ...baseOptions,
    scales: {
      ...baseOptions.scales,
      y: {
        ...baseOptions.scales?.y,
        title: { display: true, text: 'GB', color: axisColor },
      },
    },
  };

  const cpuOpts: ChartOptions<'bar'> = {
    ...baseOptions,
    scales: {
      ...baseOptions.scales,
      x: { ...baseOptions.scales?.x, stacked: false },
      y: {
        ...baseOptions.scales?.y,
        title: { display: true, text: 'CPU %', color: axisColor },
      },
    },
  };

  return (
    <div className="space-y-10">
      <p className="text-sm leading-relaxed text-ink-600 dark:text-ink-400">
        <span className="font-semibold text-ink-800 dark:text-ink-200">Live Chart.js (client-rendered)</span>
        {' — '}
        same stack as <code className="rounded bg-ink-100 px-1 font-mono text-xs dark:bg-ink-800">html-report</code>. Sample
        run <code className="font-mono text-xs">{BENCHMARK_SAMPLE_META.runId}</code>
        <span className="text-ink-400"> · </span>
        {BENCHMARK_SAMPLE_META.profile}
      </p>

      {chartCard(
        'Throughput by mode',
        'Wall-clock MB/s per phase (primary concurrency).',
        <div className="h-[min(420px,56vw)] min-h-[280px] w-full">
          <Bar data={throughputData} options={throughputOpts} />
        </div>,
      )}

      {chartCard(
        'Storage view',
        'Logical payload vs implied stored after Prometheus Δ saved.',
        <div className="h-[min(380px,52vw)] min-h-[260px] w-full">
          <Bar data={storageData} options={storageOpts} />
        </div>,
      )}

      {chartCard(
        'Docker CPU % — mean & max per mode window',
        'Mean/max of docker stats samples between before_snapshot and after_snapshot (whole-run timeseries).',
        <div className="h-[min(380px,52vw)] min-h-[260px] w-full">
          <Bar data={cpuData} options={cpuOpts} />
        </div>,
      )}
    </div>
  );
}
