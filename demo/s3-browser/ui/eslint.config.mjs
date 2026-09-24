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
/** [selector, replacement] for AntD 6 props that log a deprecation warning in dev. */
const ANTD_DEPRECATED = [
  ["JSXOpeningElement[name.name='Alert'] > JSXAttribute[name.name='message']", 'use title instead of message.'],
  ["JSXOpeningElement[name.name='Alert'] > JSXAttribute[name.name='onClose']", 'use closable={{ onClose }}.'],
  ["JSXOpeningElement[name.name=/^(Space|Steps)$/] > JSXAttribute[name.name='direction']", 'use orientation instead of direction.'],
  ["JSXOpeningElement[name.object.name='Space'][name.property.name='Compact'] > JSXAttribute[name.name='direction']", 'use orientation.'],
  ["JSXOpeningElement[name.name='Progress'] > JSXAttribute[name.name='strokeWidth']", 'use size (for example size={{ height: 2 }}).'],
  ["JSXAttribute[name.name='destroyOnClose']", 'use destroyOnHidden.'],
  ["JSXAttribute[name.name='maskClosable']", 'use mask={{ closable }}.'],
  ["JSXOpeningElement[name.name='Drawer'] > JSXAttribute[name.name=/^(width|height)$/]", 'use size.'],
  ["JSXOpeningElement[name.name='Select'] > JSXAttribute[name.name='optionFilterProp']", 'use showSearch={{ optionFilterProp }}.'],
  ["JSXMemberExpression[object.name='Dropdown'][property.name='Button']", 'Dropdown.Button: use Space.Compact + Button + Dropdown.'],
  ["JSXMemberExpression[object.name='Button'][property.name='Group']", 'Button.Group: use Space.Compact.'],
  ["JSXMemberExpression[object.name='Input'][property.name='Group']", 'Input.Group: use Space.Compact.'],
  ["JSXAttribute[name.name=/^addon(After|Before)$/]", 'use suffix/prefix, or Space.Compact.'],
  ["JSXOpeningElement[name.name='Card'] > JSXAttribute[name.name='bordered']", 'use variant.'],
  ["JSXOpeningElement[name.name='Card'] > JSXAttribute[name.name='headStyle']", 'use styles.header.'],
  ["JSXOpeningElement[name.name=/^(Card|Modal|Drawer)$/] > JSXAttribute[name.name='bodyStyle']", 'use styles.body.'],
  ["JSXOpeningElement[name.name=/^(Modal|Drawer)$/] > JSXAttribute[name.name='maskStyle']", 'use styles.mask.'],
  ["JSXOpeningElement[name.name=/^(Select|AutoComplete)$/] > JSXAttribute[name.name=/^(dropdownStyle|dropdownClassName|popupClassName|dropdownRender|onDropdownVisibleChange|dropdownMatchSelectWidth)$/]", 'use styles.popup / classNames.popup / popupRender / onOpenChange / popupMatchSelectWidth.'],
  ["JSXOpeningElement[name.name='Tabs'] > JSXAttribute[name.name='destroyInactiveTabPane']", 'use destroyOnHidden.'],
  ["JSXOpeningElement[name.name='Collapse'] > JSXAttribute[name.name='destroyInactivePanel']", 'use destroyOnHidden.'],
];

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
        {
          name: 'antd',
          importNames: ['Dropdown'],
          message: 'Use components/KeyboardDropdown: it focuses the menu on open so the arrow keys work.',
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
    {
      // Safari < 16.4 cannot parse a lookbehind: one in the bundle breaks the whole app.
      selector: 'Literal[regex.pattern=/\\(\\?<[=!]/]',
      message: 'No regex lookbehind ((?<= / (?<!): Safari < 16.4 fails to parse the bundle. Capture the prefix instead, e.g. (^|[^\\w-]).',
    },
    {
      selector: "NewExpression[callee.name='RegExp'] > Literal[value=/\\(\\?<[=!]/]",
      message: 'No regex lookbehind ((?<= / (?<!): Safari < 16.4 fails to parse the bundle.',
    },
    {
      // A row/card that handles Enter/Space itself swallowed the keys of its
      // own buttons (Enter on the Jobs "⋯" opened the drawer).
      selector: "BinaryExpression[operator=/^[!=]==?$/][left.property.name='key'][right.value=' ']",
      message: 'Use activateOnKey() from src/keyboard.ts: it ignores keys aimed at buttons inside the row.',
    },
    {
      // Destructive row actions go in <RowActionsMenu> (⋯ + confirm), not a red trash per row.
      selector:
        "JSXOpeningElement[name.name='Button']:has(JSXAttribute[name.name='danger']):has(JSXAttribute[name.name='icon'] JSXIdentifier[name='DeleteOutlined'])",
      message: 'Put destructive row actions in <RowActionsMenu> (⋯ with a confirmation), not a red trash button.',
    },
    // AntD 6 deprecated props: each one floods the dev console with a warning.
    ...ANTD_DEPRECATED.map(([selector, message]) => ({ selector, message: `AntD 6 deprecation: ${message}` })),
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
  // Dev-only page entry (like main.tsx): it mounts itself and exports
  // nothing, so Fast Refresh boundaries don't apply.
  {
    files: ['src/storyboard.tsx'],
    rules: { 'react-refresh/only-export-components': 'off' },
  },
  // The two sanctioned wrappers are the only places allowed to touch the raw APIs.
  {
    files: ['src/safeStorage.ts'],
    rules: { 'no-restricted-properties': 'off', 'no-restricted-globals': 'off' },
  },
  // The one sanctioned Enter/Space activation handler.
  {
    files: ['src/keyboard.ts'],
    rules: { 'no-restricted-syntax': 'off' },
  },
  {
    files: ['src/useCopyToClipboard.ts'],
    rules: { 'no-restricted-properties': 'off' },
  },
);
