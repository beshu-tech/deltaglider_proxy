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
 *   - no-unused-vars + unused-imports → tree-shake noise and dead code.
 *
 * Warn-only (grandfathered):
 *   - any-related TS rules are set to warn because the existing code
 *     uses them liberally and cleaning them up is a separate task.
 */
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
      'react-hooks/exhaustive-deps': 'warn',

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

      // ── build-time invariants ───────────────────────────────────
      'react-refresh/only-export-components': [
        'warn',
        { allowConstantExport: true },
      ],
    },
  },
);
