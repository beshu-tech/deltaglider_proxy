/**
 * The unsaved-changes bar floats over the Jobs drawer (so a rule edited in
 * the drawer can be applied), but the review modal that its button opens must
 * paint over the bar. AntD gives a top-level Drawer and Modal the same
 * z-index (zIndexPopupBase, 1000), so the theme lifts modals above the bar.
 */
import { render } from '@testing-library/react';
import { expect, test } from 'vitest';
import StickyDirtyBar from '../components/StickyDirtyBar';
import { darkTheme, lightTheme } from '../theme';

const ANTD_DRAWER_Z = 1000;

function barZ(): number {
  const { container } = render(
    <StickyDirtyBar visible applying={false} onDiscard={() => {}} onApply={() => {}} />,
  );
  return Number((container.firstElementChild as HTMLElement).style.zIndex);
}

test.each([['light', lightTheme], ['dark', darkTheme]])(
  '%s theme: drawer < dirty bar < modal',
  (_name, theme) => {
    const bar = barZ();
    expect(bar).toBeGreaterThan(ANTD_DRAWER_Z);
    const modalZ = (theme as { components?: { Modal?: { zIndexPopupBase?: number } } })
      .components?.Modal?.zIndexPopupBase ?? ANTD_DRAWER_Z;
    expect(modalZ).toBeGreaterThan(bar);
  },
);
