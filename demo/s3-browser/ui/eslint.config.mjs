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

/** Theme rules: colours from the palette, hover in CSS (see the CLAUDE.md styling section). */
const THEME_SYNTAX_RULES = [
    {
      // Colours come from the palette (ThemeContext.tsx → useColors() / cssVar() /
      // tint()), never from a literal: a literal skips the theme switch and the
      // contrast tests. A real exception (a mock of another product's UI) goes
      // in COLOR_LITERAL_FILES below with its reason.
      selector:
        ":matches(Literal[value=/^#([0-9a-fA-F]{3}|[0-9a-fA-F]{4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/], Literal[value=/(rgb|hsl)a?\\(/], TemplateElement[value.raw=/(rgb|hsl)a?\\(/], Literal[value=/(gradient|solid|dashed|inset|\\dpx) *[^#]*#[0-9a-fA-F]{3,8}\\b/], TemplateElement[value.raw=/(gradient|solid|dashed|inset|\\dpx) *[^#]*#[0-9a-fA-F]{3,8}\\b/])",
      message: 'No colour literal: use a palette token (useColors(), cssVar(), tint() from src/themeVars.ts). New colour? Add it to ThemeContext.tsx for both themes.',
    },
    {
      // `${color}22` builds a colour by string concatenation; it is invalid CSS for
      // anything but #rrggbb, silently.
      selector: "TemplateLiteral > TemplateElement[value.raw=/^[0-9a-fA-F]{2}(?![0-9a-zA-Z])/]:not(:first-child)",
      message: 'No hex-alpha concatenation (`${color}22`): use tint(color, percent) from src/themeVars.ts.',
    },
    {
      // A mouse handler that writes style misses keyboard focus and keeps a stale
      // colour across a theme flip.
      selector: "JSXAttribute[name.name=/^onMouse(Enter|Leave|Over|Out)$/] AssignmentExpression > MemberExpression.left[object.property.name='style']",
      message: 'Hover styling belongs in CSS: use a dg-hover-* class from theme.css, not an onMouseEnter style write.',
    },
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
      // Any string or template text that spells a lookbehind: RegExp('…'),
      // new RegExp(`…`), String.raw, a pattern kept in a constant.
      selector: ":matches(Literal[value=/\\(\\?<[=!]/], TemplateElement[value.raw=/\\(\\?<[=!]/])",
      message: 'No regex lookbehind ((?<= / (?<!): Safari < 16.4 fails to parse the bundle.',
    },
    {
      // A row/card that handles Enter/Space itself swallowed the keys of its
      // own buttons (Enter on the Jobs "⋯" opened the drawer).
      // Covers e.key === ' ', ' ' === e.key, [..., ' '].includes(e.key) and case ' ':.
      selector:
        ":matches(BinaryExpression[operator=/^[!=]==?$/][left.property.name='key'][right.value=' '], BinaryExpression[operator=/^[!=]==?$/][right.property.name='key'][left.value=' '], CallExpression[callee.property.name='includes'][arguments.0.property.name='key'][callee.object.type='ArrayExpression']:has(Literal[value=' ']), SwitchCase[test.value=' '])",
      message: 'Use activateOnKey() from src/keyboard.ts: it ignores keys aimed at buttons inside the row.',
    },
    {
      // Destructive row actions go in <RowActionsMenu> (⋯ + confirm), not a red trash per row.
      selector:
        "JSXOpeningElement[name.name='Button']:has(JSXAttribute[name.name='danger']):has(JSXAttribute[name.name='icon'] JSXIdentifier[name='DeleteOutlined'])",
      message: 'Put destructive row actions in <RowActionsMenu> (⋯ with a confirmation), not a red trash button.',
    },
    {
      // axe aria-progressbar-name: a progress bar with no name reads as "progress bar, 40%".
      selector: "JSXOpeningElement[name.name='Progress']:not(:has(JSXAttribute[name.name='aria-label']))",
      message: 'Give <Progress> an aria-label that says what is progressing.',
    },
    {
      // A hand-picked fill on a Button skips the contrast that the theme tokens
      // are tested for (the amber password button was 2.89:1). Use type/danger.
      selector:
        "JSXOpeningElement[name.name='Button'] JSXAttribute[name.name='style'] Property[key.name=/^(background|backgroundColor)$/]:matches([value.name=/^ACCENT_/], [value.property.name=/^ACCENT_/])",
      message: 'No ACCENT_* fill on <Button>: use type="primary" / danger, whose colours contrast.test.ts checks.',
    },
    ...THEME_SYNTAX_RULES,
    // AntD 6 deprecated props: each one floods the dev console with a warning.
    ...ANTD_DEPRECATED.map(([selector, message]) => ({ selector, message: `AntD 6 deprecation: ${message}` })),
  ],
  'no-restricted-globals': [
    'error',
    { name: 'localStorage', message: 'Use readStorage/writeStorage from src/safeStorage.ts.' },
    { name: 'sessionStorage', message: 'Use readStorage/writeStorage(..., "session") from src/safeStorage.ts.' },
    { name: 'confirm', message: 'Use confirmDialog from src/confirmDialog.ts.' },
  ],
  'no-restricted-properties': [
    'error',
    { object: 'window', property: 'localStorage', message: 'Use src/safeStorage.ts.' },
    { object: 'window', property: 'sessionStorage', message: 'Use src/safeStorage.ts.' },
    { object: 'window', property: 'confirm', message: 'Use confirmDialog from src/confirmDialog.ts: window.confirm blocks the page and ignores the theme.' },
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
  // Colour literals are allowed only here, each for a stated reason:
  //   the palette and AntD's own tokens; tests and the screenshot annotations;
  //   the Slack mocks (they draw Slack's own UI, whose colours are not ours);
  //   the media viewers (an image on black, an HTML page on white, a dark scrim);
  //   the white sheen on the savings bars (the same in both themes).
  {
    files: [
      'src/ThemeContext.tsx', 'src/theme.ts', 'src/__tests__/**', 'src/test/**', 'e2e/**',
      'src/components/SlackSetupGuide.tsx', 'src/components/SlackConnectorCard.tsx',
      'src/components/FilePreview.tsx', 'src/components/HtmlPreview.tsx', 'src/components/Lightbox.tsx',
      'src/components/HeroSavingsPanel.tsx', 'src/components/BucketFleetList.tsx',
    ],
    rules: {
      'no-restricted-syntax': ['error', ...UI_RULES['no-restricted-syntax'].slice(1).filter((r) => !THEME_SYNTAX_RULES.includes(r))],
    },
  },
  // Tests stub the raw storage and clipboard APIs the wrappers guard.
  {
    files: ['src/__tests__/**', 'src/test/**'],
    rules: {
      'no-restricted-properties': 'off',
      'no-restricted-globals': 'off',
      'react-refresh/only-export-components': 'off',
    },
  },
  {
    files: ['src/useCopyToClipboard.ts'],
    rules: { 'no-restricted-properties': 'off' },
  },
  // Playwright specs: page.addInitScript / evaluate bodies run in the page,
  // where src/safeStorage.ts cannot be imported. They wrap raw storage in
  // try/catch themselves.
  {
    files: ['e2e/**'],
    rules: { 'no-restricted-properties': 'off', 'no-restricted-globals': 'off' },
  },
);
