interface ProofItem {
  label: string;
}

const ITEMS: readonly ProofItem[] = [
  { label: 'Transparent S3 proxy' },
  { label: 'xdelta3 dedupe' },
  { label: 'ABAC IAM + OAuth' },
  { label: 'Soft bucket quotas' },
  { label: 'Object replication' },
  { label: 'Prometheus dashboards' },
];

export function ProofStrip(): JSX.Element {
  return (
    <section className="border-y border-ink-200 bg-white/70 py-6 dark:border-ink-700 dark:bg-ink-900/60">
      <div className="mx-auto flex max-w-6xl flex-wrap items-center justify-center gap-x-8 gap-y-2 px-6 text-sm font-semibold text-ink-700 dark:text-ink-300">
        {ITEMS.map((item, idx) => (
          <span
            key={item.label}
            className="flex items-center gap-2"
          >
            {idx > 0 && (
              <span aria-hidden className="text-ink-300 dark:text-ink-600">
                ·
              </span>
            )}
            <span>{item.label}</span>
          </span>
        ))}
      </div>
    </section>
  );
}
