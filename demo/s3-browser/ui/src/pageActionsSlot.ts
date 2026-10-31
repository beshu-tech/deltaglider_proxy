import { createContext } from 'react';

/** The DOM node at the right of the page header (TabHeader) that
 *  <PageActions> portals into; null outside an admin page. */
export const PageActionsSlot = createContext<HTMLElement | null>(null);
