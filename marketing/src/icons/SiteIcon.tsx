import type { LucideIcon } from 'lucide-react';
import { LUCIDE_STROKE } from './sizes';

interface SiteIconProps {
  icon: LucideIcon;
  className?: string;
  'strokeWidth'?: number;
  'aria-label'?: string;
}

/**
 * All Lucide icons: single stroke, optional label for standalone icons.
 */
export function SiteIcon({
  icon: Icon,
  className,
  strokeWidth = LUCIDE_STROKE,
  'aria-label': label,
}: SiteIconProps): JSX.Element {
  return (
    <Icon
      className={className}
      strokeWidth={strokeWidth}
      aria-hidden={label ? undefined : true}
      aria-label={label}
    />
  );
}
