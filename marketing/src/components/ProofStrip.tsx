interface ProofItem {
  label: string;
}

const ITEMS: readonly ProofItem[] = [
  { label: 'Encryption at rest' },
  { label: 'SigV4 drop-in' },
  { label: 'ABAC IAM' },
  { label: 'Prometheus metrics' },
  { label: 'Single binary' },
  { label: 'Open source' },
];

export function ProofStrip(): JSX.Element {
  return (
    <section className="border-y border-ink-200 bg-white/60 py-6 dark:border-ink-700 dark:bg-ink-900/40">
      <div className="mx-auto max-w-5xl px-6 flex flex-wrap items-center justify-center gap-x-8 gap-y-2 text-sm font-semibold text-ink-700 dark:text-ink-300">
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
