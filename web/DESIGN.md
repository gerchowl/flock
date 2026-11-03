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
