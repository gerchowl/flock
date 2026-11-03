# Client design rules

The web client (`web/client`) looks like a macOS app: system font, system
colours, a 4 px grid. The rules below are enforced, not advice. `just lint`
and `just client-check` fail on a violation. Apple's Human Interface
Guidelines (HIG) are the reference; each section cites the pages it follows.

## Tokens

Every colour, font, spacing step, radius, control size and layer is a design
token. The sources are `web/client/tokens/*.json` in the
[DTCG 2025.10](https://www.designtokens.org/tr/2025.10/format/) format, and
Style Dictionary v5 compiles them into two committed files:

- `src/tokens.css`: CSS custom properties, `var(--space-3)`;
- `src/tokens.ts`: `Token`, the union of every token name, and `SpaceStep`.

| Role | Tokens | Notes |
| --- | --- | --- |
| Font | `--font-family-system`, `--font-family-mono` | The platform's own fonts, never a web font. |
| Type scale | `--font-size-10` … `--font-size-22` | 10/11/12/13/15/17/22 px; 13 is body, 10 the minimum. |
| Weight | `--font-weight-regular` (400), `-medium` (510), `-semibold` (600) | |
| Spacing | `--space-1` … `--space-12` | The step times 4 px: 4, 8, 12, 16, 20, 24, 32, 40, 48. The generator rejects any other value or name. |
| Radius | `--radius-small` (6), `-medium` (8), `-large` (10) | Controls, cards, popovers and sheets. |
| Control | `--size-control` | 28 px on desktop, 44 px below the phone breakpoint. |
| Border | `--border-width-thin` | 1 px separators and outlines. |
| Layer | `--layer-sticky`, `--layer-overlay` | z-index, lowest first. |
| Text | `--color-label-primary`, `-secondary`, `-tertiary` | Tertiary is for disabled text only. |
| Selected text | `--color-label-on-selection`, `-on-selection-secondary` | Content of an emphasised selected row. |
| Surfaces | `--color-background-window`, `--color-background-sidebar`, `--color-separator` | |
| Accent | `--color-accent-fill`, `--color-accent-on`, `--color-link` | Accent #0a6cdc; the link is a shade darker (#0262cd in light mode). |
| Selection | `--color-selection-active`, `--color-selection-inactive`, `--color-focus-ring` | Inactive: an unfocused window or pane. |
| Meaning | `--color-destructive`, `--color-status-green`, `-orange`, `-red` | |
| Legacy | `--color-legacy-*` | The pre-redesign palette. Do not use in new code. |

The colours come in four modes: light, dark (`prefers-color-scheme`), and
each with increased contrast (`prefers-contrast: more`). Dark mode defines
every colour; the contrast modes override only the colours they change.

### Colour

`tests/tokens.test.ts` checks WCAG 2.2 AA on the full matrix, in every mode.
The surfaces are `background.window`, `background.sidebar`,
`selection.inactive`, `selection.active` and `accent.fill`, all opaque.
Every other colour is content, and each content colour is checked on each
surface: 4.5:1 for text (SC 1.4.3), 3:1 for the focus ring (SC 1.4.11).
Status and destructive colours are text: they reach 4.5:1 wherever they
render. The pairs are never listed by hand, so a new colour is checked on
every surface. `EXEMPT` in the test is the only way out: each entry names
the colours, the surfaces and the reason, must exempt at least one failing
pair, and overlaps no other entry. Today it exempts:

- `label.tertiary`: disabled text, which SC 1.4.3 exempts;
- `separator`: decorative hairlines;
- ordinary content on `selection.active` and `accent.fill`, which take the
  alternate colours instead (below);
- the alternate colours on every other surface, where they never render;
- the focus ring on the emphasised surfaces: it is drawn outside the
  element (`outline-offset`), on the surface around it;
- `legacy.*`, which renders only on its own legacy backgrounds.

**Selected rows follow the HIG.** An emphasised selection
(`selection.active`, the selected agent, the palette's highlighted item)
draws all of its content in the alternate colours:
`label.on-selection` for text and glyphs, status marks included, and
`label.on-selection-secondary` for secondary text. An unemphasised
selection (`selection.inactive`) keeps the ordinary labels, link, status
and destructive colours, so those reach 4.5:1 on it too. Sources:
[`alternateSelectedControlTextColor`](https://developer.apple.com/documentation/appkit/nscolor/alternateselectedcontroltextcolor),
[`selectedContentBackgroundColor`](https://developer.apple.com/documentation/appkit/nscolor/selectedcontentbackgroundcolor),
[`unemphasizedSelectedContentBackgroundColor`](https://developer.apple.com/documentation/appkit/nscolor/unemphasizedselectedcontentbackgroundcolor)
and the [emphasized background style](https://developer.apple.com/documentation/appkit/nsview/backgroundstyle/emphasized).

### Adding or changing a token

1. Edit `tokens/base.json` (type, spacing, sizes), `tokens/color/light.json`
   and `tokens/color/dark.json` (a new colour needs both), the
   `*-contrast.json` overrides, or `tokens/phone.json`. Give it a
   `$description` that says when to use it, citing the HIG page.
2. Run `bun run tokens` in `web/client` and commit the JSON together with
   both generated files. `bun run tokens:check` fails while they are stale.
3. A new colour is content and is checked on every surface. A new
   background goes in `SURFACES` in `tests/tokens.test.ts`; a non-text
   colour goes in `GRAPHICS`. Exempt a pair only for a reason a reviewer
   can check.
4. A spacing token is `space.<n>` with the value n × 4 px. The generator
   fails on anything else, so `SpaceStep` holds only on-scale steps.

Add a token when its first use needs it, and only on the scale: a value off
the scale is a design question, not a missing token.

## Rules

1. **No raw values.** Colour, background, border, radius, `font-*`,
   `line-height`, `box-shadow`, `z-index`, gaps, margins and padding take a
   token or a keyword (`0`, `auto`, `inherit`, `currentColor`, `none`, …).
   Raw values appear only in the generated `tokens.css`. A custom property
   may only alias tokens: `--x: var(--space-2)`, never `--x: 8px`.
   (stylelint, `stylelint-declaration-strict-value`.)
2. **Layout lives in `src/layout/`.** Outside it, CSS may not use
   `display: flex` or `grid` (inline forms included), `gap`, margins,
   `position: absolute` or `@media`. Compose the primitives instead.
   (stylelint overrides.)
3. **No inline styles.** TSX outside `src/layout/` may not set `style=` (or
   `attr:style`/`prop:style`): an inline style escapes both rules above.
   (ESLint.)
4. **Spacing is typed.** A primitive's `space` prop is a `SpaceStep`, so
   `<Stack space="14">` is a type error. (tsc.)
5. **Token names are reserved.** Only the generated `tokens.css` declares a
   custom property in a token category (`--color-*`, `--space-*`,
   `--font-*`, `--radius-*`, `--size-*`, `--border-*`, `--layer-*`), as a
   declaration or an `@property`, so no component can rebind a token. The
   categories come from `tokens.css`, and CSS escapes are resolved first.
   This holds in the LEGACY files too. (stylelint,
   `flock/no-token-redeclaration` in `scripts/stylelint-reserved-tokens.js`.)

`tests/design-rules.test.ts` seeds a violation of each rule and expects it to
fail, and expects the tree to pass; `tests/tokens.test.ts` does the same for
rule 5, the spacing check and the contrast matrix.

### The temporary ignore list

`LEGACY` in `web/client/stylelint.config.js` lists the stylesheets written
before the tokens. They are held to rule 1 for colours only and are exempt
from rule 2. Their colours use `--color-legacy-*`; their other literals
(rem-based spacing on a 14 px root, 14 px body text, 44 px desktop controls)
have no exact token, and replacing them changes how the GUI looks. That is
the redesign's work, one file at a time. When a file passes every rule,
remove it from the list. Never add one.

## Conformance

The lint rules above check the source. `e2e/conformance.test.ts` checks what
the browser renders: it walks every element with `getComputedStyle` on the
gallery (`gallery.html`, every primitive and control state; `vite` serves it
in development, `vite build --mode gallery` builds it, and no production
build includes it) and on the fixture workbench at desktop and phone sizes,
including `?layout=phone`. It runs in light, dark and both
increased-contrast modes, all under reduced motion, in `just client-check`,
so the required `client` CI check runs it on every change. One test per rule:

1. **Palette.** Every computed `color`, `background-color`, drawn
   `border-*-color` and `outline-color`, and SVG `fill` and `stroke` equals a
   `--color-*` token as the active mode resolves it (read from the generated
   `tokens.css` in the page), or is transparent.
   ([Color](https://developer.apple.com/design/human-interface-guidelines/color):
   system and semantic colours, adapting to the mode.)
2. **Typography.** Desktop body text is 13 px; no text is under 10 px; no
   weight under 300 below 15 px; every font is the system or monospaced stack.
   ([Typography](https://developer.apple.com/design/human-interface-guidelines/typography):
   macOS body 13 pt, minimum 10 pt, avoid thin weights at small sizes.)
3. **Contrast.** Text has 4.5:1 against its background with every ancestor's
   background composited beneath it, 3:1 when large (24 px, or 18.66 px
   bold), in every mode. Disabled controls are exempt. axe-core's WCAG 2.2 A
   and AA rules run beside it.
   ([WCAG 2.2 SC 1.4.3](https://www.w3.org/TR/WCAG22/#contrast-minimum);
   [Accessibility](https://developer.apple.com/design/human-interface-guidelines/accessibility).)
4. **Hit size.** Every control is at least 28×28 px on desktop and 44×44 px
   on a phone; 20×20 px inside `[data-control-size="small"]`. Every control
   is measured, below the fold too; only links in running text are exempt.
   Neighbouring controls keep 4 px apart, except rows of one list (`ul`,
   `ol`, `role="list"`, `listbox`, `tree`, `menu`) and segments of one
   `tablist` or `group`, which may touch. A control covered by another even
   when scrolled into view, or partly scrolled out of its scroller, has no
   defined neighbour and is skipped for spacing only.
   ([Buttons](https://developer.apple.com/design/human-interface-guidelines/buttons);
   [WCAG 2.2 SC 2.5.8](https://www.w3.org/TR/WCAG22/#target-size-minimum).)
5. **Spacing grid.** Every computed margin, padding and gap in px is a
   multiple of 4, or the 1 px hairline. `auto` and percentages are skipped.
   ([Layout](https://developer.apple.com/design/human-interface-guidelines/layout).)
6. **Materials.** `backdrop-filter` or a blur appears only inside chrome (a
   header, toolbar, sidebar, dialog or popover), never inside `main`.
   ([Materials](https://developer.apple.com/design/human-interface-guidelines/materials):
   materials belong to the controls layer, not the content.)
7. **Motion.** Under reduced motion nothing transitions or animates except
   `opacity`.
   ([Motion](https://developer.apple.com/design/human-interface-guidelines/motion);
   [WCAG 2.2 SC 2.3.3](https://www.w3.org/TR/WCAG22/#animation-from-interactions).)
8. **Keyboard.** Tab reaches every enabled control (a list's rows are reached
   with the arrow keys instead), and each shows a focus ring: an outline
   drawn only on focus, or a box-shadow that differs from its resting one.
   Every control has an accessible name with a letter or digit, so an icon
   alone is not one. ArrowDown moves the selection of every list widget,
   the palette's included, and a list with nothing to focus fails. ⌘K opens
   the palette from wherever focus is, and Esc closes it and returns focus
   there.
   ([Keyboards](https://developer.apple.com/design/human-interface-guidelines/keyboards);
   [WCAG 2.2 SC 2.1.1, 2.4.7, 4.1.2](https://www.w3.org/TR/WCAG22/).)
9. **Structure.** A sidebar's disclosure groups nest at most two deep. On
   every screen with a toolbar (a `header` or `role="toolbar"`), each
   toolbar has actions and every one of them, disabled ones included, is a
   ⌘K command: the palette lists an unavailable action dimmed. No control
   switches the app's light or dark appearance, which follows the system.
   ([Sidebars](https://developer.apple.com/design/human-interface-guidelines/sidebars);
   [Toolbars](https://developer.apple.com/design/human-interface-guidelines/toolbars);
   [Dark Mode](https://developer.apple.com/design/human-interface-guidelines/dark-mode).)

The keyboard and structure checks that drive the page (Tab, arrows, ⌘K and
Esc, toolbar parity) run in every mode too. Nothing passes by running less:
a coverage test fails unless every page ran in every mode, as the page's own
media queries report it, and each walk covered the page's minimum of
elements and controls. Closing the browser is bounded too: a cleanup that
times out fails the run, and the browser's processes are killed and reaped
either way.

Every check, and each part of rules 4, 8 and 9, has a seeded violation in
the test's `SEEDS` (20 in all: one below the fold, one only in dark mode,
one hiding the page). A test runs each check on its page with and without
its seed and fails unless the seed adds a finding.
`CONFORMANCE_SEED=<seed> bun test ./e2e/conformance.test.ts` injects one
seed for the whole run instead, and its rule's test fails. The suite's own
seeds (`SUITE_SEEDS`: a broad or unbaselined allow-list entry, a dropped
mode, a cleanup that never settles) fail the run the same way.

Known violations live in one list, `e2e/conformance-allow.ts`. Each entry
names one rule, one simple selector, the pages and the filed issue that
removes it. A selector is a tag, classes, ids or an exact test id, with at
most two more such compounds down a descendant chain: no `*`, no selector
list, no pseudo-class such as `:is`, `:not` or `:where`, no other attribute
match, and no chain rooted at `html`, `body` or `#root` (`body` alone names
the body element). The test fails on an entry that no longer matches
anything, and on an entry, or a page of one, that is not in the bootstrap
baseline (`e2e/conformance-allow-baseline.ts`, the list as first merged) or,
once `dev` has the list, in `origin/dev`'s copy, so the list only shrinks;
CI fetches `dev` for that comparison. Today it holds the legacy
stylesheets' body size, count badge, rem spacing and touching sidebar and
tab-bar rows (#2913), and the sidebar's third level (#2930).

## Layout

The primitives in `src/layout/primitives/` follow
[Every Layout](https://every-layout.dev/layouts/):

| Primitive | Does | Props |
| --- | --- | --- |
| `Stack` | Children in a column. | `space` |
| `Cluster` | A wrapping row: toolbars, tags, buttons. | `space`, `justify`, `align` |
| `Sidebar` | A fixed side beside fluid content, stacking when narrow. | `side`, `sideWidth`, `contentMin`, `space` |
| `Switcher` | One row above a width threshold, one column below. | `threshold`, `space` |
| `Center` | A centred column with a maximum width. | `max`, `gutters`, `intrinsic` |
| `Grid` | As many equal columns as fit. | `min`, `space` |

Each takes `as` (the element), `class` and the element's other attributes.
They respond to their container, not the viewport, so screens need no media
queries.

## HIG references

- [Typography](https://developer.apple.com/design/human-interface-guidelines/typography):
  the system font, the macOS type scale, 13 px body.
- [Color](https://developer.apple.com/design/human-interface-guidelines/color):
  semantic colours, the accent only for selection, focus and the one primary
  action, and dark and increased-contrast variants. Selected content uses
  the alternate colours (see "Colour").
- [Layout](https://developer.apple.com/design/human-interface-guidelines/layout):
  consistent spacing and alignment.
- [Buttons](https://developer.apple.com/design/human-interface-guidelines/buttons):
  28 px regular controls; a destructive action is red and never the default.
- [Accessibility](https://developer.apple.com/design/human-interface-guidelines/accessibility):
  contrast, 44 px touch targets, visible keyboard focus.
