// @ts-check
import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';
import globals from 'globals';

/**
 * Blocking CI rules — every error here fails CI.
 *
 * Non-negotiable:
 *   - react-hooks/rules-of-hooks → prevents Error #310 regressions
 *     (that crash shipped to prod once; never again).
 *   - react-hooks/exhaustive-deps → stale-closure class of bugs.
 *   - app UI rules (see UI_RULES below): no AntD Tooltip/Popover (theme.css
 *     hides them, so their content is invisible), clipboard only through
 *     useCopyToClipboard, Web Storage only through safeStorage.
 *   - no-unused-vars + unused-imports → tree-shake noise and dead code.
 *
 * Warn-only (grandfathered):
 *   - any-related TS rules are set to warn because the existing code
 *     uses them liberally and cleaning them up is a separate task.
 */
const UI_RULES = {
  'no-restricted-imports': [
    'error',
    {
      paths: [
        {
          name: 'antd',
          importNames: ['Tooltip', 'Popover'],
          message: 'theme.css hides every AntD tooltip/popover — use a native title or <HoverHint>.',
        },
      ],
    },
  ],
  'no-restricted-syntax': [
    'error',
    {
      selector: "JSXAttribute[name.name='ellipsis'] Property[key.name='tooltip']",
      message: 'ellipsis.tooltip renders an AntD tooltip, which theme.css hides — pass a native title instead.',
    },
  ],
  'no-restricted-globals': [
    'error',
    { name: 'localStorage', message: 'Use readStorage/writeStorage from src/safeStorage.ts.' },
    { name: 'sessionStorage', message: 'Use readStorage/writeStorage(..., "session") from src/safeStorage.ts.' },
  ],
  'no-restricted-properties': [
    'error',
    { object: 'window', property: 'localStorage', message: 'Use src/safeStorage.ts.' },
    { object: 'window', property: 'sessionStorage', message: 'Use src/safeStorage.ts.' },
    {
      object: 'navigator',
      property: 'clipboard',
      message: 'Use useCopyToClipboard — navigator.clipboard is undefined on plain HTTP.',
    },
  ],
};

export default tseslint.config(
  {
    ignores: ['dist/**', 'node_modules/**', 'coverage/**'],
  },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ['**/*.{ts,tsx}'],
    plugins: {
      'react-hooks': reactHooks,
      'react-refresh': reactRefresh,
    },
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: 'module',
      globals: {
        ...globals.browser,
        ...globals.es2022,
      },
      parserOptions: {
        ecmaFeatures: { jsx: true },
      },
    },
    rules: {
      // ── blocking: hook correctness ──────────────────────────────
      'react-hooks/rules-of-hooks': 'error',
      'react-hooks/exhaustive-deps': 'error',

      // ── blocking: dead code / unused ────────────────────────────
      '@typescript-eslint/no-unused-vars': [
        'error',
        {
          args: 'none', // Too many (props, handlers) — argsIgnorePattern covers intentional cases
          varsIgnorePattern: '^_',
          argsIgnorePattern: '^_',
          caughtErrorsIgnorePattern: '^_',
        },
      ],
      'no-unused-vars': 'off', // TS version supersedes

      // ── warn: TS/any ergonomics (cleanup later) ─────────────────
      '@typescript-eslint/no-explicit-any': 'warn',
      '@typescript-eslint/no-empty-object-type': 'warn',
      '@typescript-eslint/ban-ts-comment': 'warn',

      ...UI_RULES,

      // ── build-time invariants ───────────────────────────────────
      'react-refresh/only-export-components': [
        'warn',
        { allowConstantExport: true },
      ],
    },
  },
  // The two sanctioned wrappers are the only places allowed to touch the raw APIs.
  {
    files: ['src/safeStorage.ts'],
    rules: { 'no-restricted-properties': 'off', 'no-restricted-globals': 'off' },
  },
  {
    files: ['src/useCopyToClipboard.ts'],
    rules: { 'no-restricted-properties': 'off' },
  },
);
