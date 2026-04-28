import { lazy, Suspense, useEffect, useState, type ReactNode } from 'react';

const BenchmarkChartsBody = lazy(async () => import('./BenchmarkChartsBody'));

function ChartSkeleton(): ReactNode {
  return (
    <div className="space-y-8">
      {[1, 2, 3].map((key) => (
        <div
          key={key}
          className="h-[min(380px,52vw)] min-h-[260px] animate-pulse rounded-2xl border border-ink-200 bg-gradient-to-br from-ink-100/90 to-ink-50 dark:border-ink-700 dark:from-ink-800/80 dark:to-ink-900/60"
          aria-hidden
        />
      ))}
    </div>
  );
}

/**
 * Chart.js runs only in the browser — static prerender cannot paint canvases.
 * After mount we lazy-load the chart bundle so SSR/SSG never imports Chart.js.
 */
export function BenchmarkInteractiveCharts(): JSX.Element {
  const [mounted, setMounted] = useState(false);

  useEffect(() => {
    setMounted(true);
  }, []);

  if (!mounted) {
    return <ChartSkeleton />;
  }

  return (
    <Suspense fallback={<ChartSkeleton />}>
      <BenchmarkChartsBody />
    </Suspense>
  );
}
