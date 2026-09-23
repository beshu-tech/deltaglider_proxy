import { useEffect } from 'react';
import { isSessionExpired } from '../errorHandling';

/**
 * Call `onSessionExpired` when a react-query `error` is a session expiry.
 * An effect, not render-body code: react-query keeps `error` populated across
 * renders, so calling the (navigating) callback during render would set state
 * during render.
 */
export function useSessionExpiredOn(error: unknown, onSessionExpired?: () => void): void {
  useEffect(() => {
    if (isSessionExpired(error)) onSessionExpired?.();
  }, [error, onSessionExpired]);
}
