interface ChecklistGridProps {
  items: readonly string[];
  columns?: 'two' | 'three';
}

const columnsClass = {
  two: 'sm:grid-cols-2',
  three: 'sm:grid-cols-2 lg:grid-cols-3',
} as const;

export function ChecklistGrid({
  items,
  columns = 'two',
}: ChecklistGridProps): JSX.Element {
  return (
    <div className={`grid gap-3 ${columnsClass[columns]}`}>
      {items.map((item) => (
        <div
          key={item}
          className="rounded-xl border border-ink-200 bg-white px-4 py-3 text-sm font-semibold text-ink-800 dark:border-ink-700 dark:bg-ink-800/50 dark:text-ink-100"
        >
          <span className="mr-2 text-brand-600 dark:text-brand-300">✓</span>
          {item}
        </div>
      ))}
    </div>
  );
}
