/**
 * PageActions — THE place of a page's primary action, and THE button
 * taxonomy of the admin UI (UX harvest D36, D69).
 *
 * Roles (one rule for every page):
 *
 *   primary    `type="primary"`. ONE per page, at the top right of the page
 *              header, through <PageActions>. It is the page's main verb,
 *              almost always "New <thing>" with <PlusOutlined />:
 *                <PageActions>
 *                  <Button type="primary" icon={<PlusOutlined />} onClick={…}>New user</Button>
 *                </PageActions>
 *              A form's submit ("Create group", "Save") is the primary of
 *              that form, at its bottom right; a modal's OK is its primary.
 *   secondary  default (outlined) Button. Toolbar actions (Refresh, Export),
 *              and "add" actions INSIDE a card (`size="small"`).
 *   tertiary   `type="text"` or `type="link"`: low-weight actions (Details,
 *              Clear filter, Cancel next to a primary).
 *   danger     `danger`: destructive actions only, always behind a confirm
 *              (`confirmDialog`). Row deletes go in <RowActionsMenu>.
 *   dashed     `type="dashed" block`: ONLY for "add a row" inside a form
 *              (<RowListEditor>), never for a page's create action.
 *
 * Not allowed: a split button for create, a primary inside a card header,
 * a second primary on the page. The empty state of a page with a header
 * primary does not repeat it (two "New user" buttons are ambiguous); its
 * hint names it ("Click New user at the top right."). A page WITHOUT a
 * header primary (the file browser) puts its actions in <EmptyState action
 * secondaryAction>.
 *
 * Outside an admin page (no header slot: a test, the setup wizard) the
 * children render in place.
 */
import { useContext, type ReactNode } from 'react';
import { createPortal } from 'react-dom';
import { PageActionsSlot } from '../pageActionsSlot';

export default function PageActions({ children }: { children: ReactNode }) {
  const slot = useContext(PageActionsSlot);
  if (!slot) return <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8 }}>{children}</div>;
  return createPortal(children, slot);
}
