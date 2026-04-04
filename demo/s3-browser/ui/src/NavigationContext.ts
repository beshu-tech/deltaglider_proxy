import { createContext, useContext } from 'react';

interface NavigationState {
  /** Navigate to a path relative to /_/ (e.g., '/admin/users', '/docs/configuration') */
  navigate: (path: string, replace?: boolean) => void;
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
