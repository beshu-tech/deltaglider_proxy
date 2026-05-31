import { createContext, useContext } from 'react';

interface NavigationState {
  /**
   * Low-level navigation. `url` is a full BASE-prefixed path (+ optional query)
   * as produced by `buildViewUrl` / `buildBrowserUrl` (a bare relative path is
   * tolerated and BASE-prefixed). Pushes a history entry by default; pass
   * `{ replace: true }` to swap the current entry instead.
   */
  navigate: (url: string, opts?: { replace?: boolean }) => void;
  /** Sub-path after the view segment (e.g., 'users' when at /_/admin/users) */
  subPath: string;
}

export const NavigationContext = createContext<NavigationState>({
  navigate: () => {},
  subPath: '',
});

export function useNavigation() {
  return useContext(NavigationContext);
}
