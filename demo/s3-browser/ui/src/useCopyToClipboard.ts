/**
 * Single home for "copy text to the clipboard" across the admin UI.
 *
 * Before this hook the same logic was hand-rolled in three places
 * (CopySectionYamlButton, YamlImportExportModal, SetupWizard's ReviewStep)
 * with subtly different behaviour — one silently swallowed failures, one
 * never reported success, only one had a download fallback.
 *
 * Contract:
 *   - `copy(text, opts?)` writes via `navigator.clipboard.writeText`, or the
 *     legacy `execCommand('copy')` outside a secure context (plain HTTP).
 *   - On success: surfaces `message.success` and flips `copied` to true
 *     for `resetMs` (default 1800ms), then back to false.
 *   - On failure / missing Clipboard API: surfaces `message.error` so the
 *     operator always gets feedback. If `fallbackFilename` is supplied, a
 *     Blob download of the text is triggered as a last resort.
 *   - Returns whether the copy succeeded so callers can branch if needed.
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { message } from 'antd';
import { normalizeUiError } from './errorHandling';

interface CopyOptions {
  /** Toast shown on a successful clipboard write. */
  successMessage?: string;
  /** If set, a failed/unavailable clipboard write falls back to a Blob download with this filename. */
  fallbackFilename?: string;
  /** MIME type for the fallback download Blob. Defaults to text/plain. */
  fallbackMimeType?: string;
  /** How long the `copied` flag stays true after a success. Defaults to 1800ms. */
  resetMs?: number;
}

function downloadText(text: string, filename: string, mimeType: string): void {
  const blob = new Blob([text], { type: mimeType });
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
  } finally {
    URL.revokeObjectURL(url);
  }
}

/**
 * `navigator.clipboard` exists only in a secure context (HTTPS or
 * localhost). A proxy reached over plain HTTP on a LAN/VPN address has
 * none, so fall back to the legacy `execCommand('copy')` on a hidden
 * textarea, which browsers still honour inside a user gesture. The textarea
 * goes next to the focused element so an open modal's focus trap keeps it.
 */
async function writeClipboard(text: string): Promise<void> {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      /* permission denied / not focused — try the legacy path */
    }
  }
  const active = document.activeElement as HTMLElement | null;
  const host = active?.closest('.ant-modal, .ant-drawer') ?? document.body;
  const ta = document.createElement('textarea');
  ta.value = text;
  ta.setAttribute('readonly', '');
  ta.style.cssText = 'position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;';
  host.appendChild(ta);
  ta.select();
  let ok: boolean;
  try {
    ok = document.execCommand('copy');
  } finally {
    ta.remove();
    active?.focus?.();
  }
  if (!ok) throw new Error('the browser blocked clipboard access');
}

export function useCopyToClipboard() {
  const [copied, setCopied] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (timerRef.current) clearTimeout(timerRef.current);
    };
  }, []);

  const copy = useCallback(async (text: string, opts: CopyOptions = {}): Promise<boolean> => {
    const {
      successMessage = 'Copied to clipboard',
      fallbackFilename,
      fallbackMimeType = 'text/plain',
      resetMs = 1800,
    } = opts;

    const fallback = (warn: string) => {
      if (fallbackFilename) {
        message.warning(`${warn} — falling back to a download.`);
        downloadText(text, fallbackFilename, fallbackMimeType);
      } else {
        message.error(warn);
      }
    };

    try {
      await writeClipboard(text);
      if (!mountedRef.current) return true;
      message.success(successMessage);
      setCopied(true);
      if (timerRef.current) clearTimeout(timerRef.current);
      timerRef.current = setTimeout(() => {
        if (mountedRef.current) setCopied(false);
      }, resetMs);
      return true;
    } catch (e) {
      fallback(`Copy failed (${normalizeUiError(e, 'unknown error')}) — select the text and copy it manually`);
      return false;
    }
  }, []);

  return { copy, copied };
}
