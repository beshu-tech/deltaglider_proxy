/**
 * Pure rule-name helper shared by the Lifecycle and Replication payload
 * modules' `emptyRule` factories.
 *
 * React-free (no antd / no hooks) so the unit test can
 * transpile-and-import it directly. Both panels seed a new draft rule with
 * a `<base>-<n>` name and bump `n` until it does not collide with an
 * existing rule name — this collapses that loop into one tested helper.
 */

/**
 * Return the first `<base>-<n>` name (n starting at `existing.length + 1`)
 * that is not already used by a rule in `existing`. Matches the historical
 * per-panel loop exactly: it starts at the count + 1 and increments until
 * the name is free, so a freshly-added rule on a list of N gets `<base>-(N+1)`
 * unless that collides.
 */
export function nextUniqueRuleName(
  existing: ReadonlyArray<{ name: string }>,
  base: string
): string {
  let n = existing.length + 1;
  let name = `${base}-${n}`;
  while (existing.some((r) => r.name === name)) {
    n += 1;
    name = `${base}-${n}`;
  }
  return name;
}
