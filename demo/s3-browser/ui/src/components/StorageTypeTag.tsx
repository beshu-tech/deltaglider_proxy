import { Tag } from 'antd';
import { useColors } from '../ThemeContext';
import { savingsColors } from '../savings';
import { storageChip } from '../deltaSummary';

type StorageTypeColor = { bg: string; border: string; text: string };

interface Props {
  storageType?: string;
  /** The folder's baseline (a delta stored whole): the neutral "Baseline" chip. */
  isBaseline?: boolean;
}

/**
 * Renders the storage-type compression badge (Original / Delta / Reference).
 * No storage type (the listing had no facts for the row) renders a neutral
 * "—" badge.
 *
 * One chip family (UX harvest D14, D58): a Delta row is a saving, so it
 * takes THE savings green (`savingsColors`, the same as the header pill and
 * the analytics hero); Original and Reference are neutral. Before, the
 * three kinds had three unrelated hues (violet, sky, green).
 */
export default function StorageTypeTag({ storageType, isBaseline = false }: Props) {
  const colors = useColors();
  const { label, title } = storageChip(storageType, isBaseline);
  const c: StorageTypeColor =
    storageType === 'delta' && !isBaseline
      ? savingsColors(colors)
      : { text: colors.TEXT_SECONDARY, bg: 'transparent', border: colors.BORDER };
  return (
    <Tag title={title} style={{
      background: c.bg,
      border: `1px solid ${c.border}`,
      color: c.text,
      borderRadius: 6,
      fontFamily: 'var(--font-mono)',
      fontSize: 11,
      fontWeight: 500,
    }}>
      {label}
    </Tag>
  );
}
