# Exemplar baseline for the client's design budgets

This spike (#2899) measures what well-designed productivity web UIs actually
render, compares the web client against them, and proposes budget tests for
the conformance suite (#2896) and calibrations of the tokens (#2895). It
measures and reports; it changes nothing in the client.

Measured 2026-11-04 in headless Chromium at 1280×800, light and dark
(`prefers-color-scheme`), under reduced motion. The raw numbers are in
`results/baseline.json`.

## How it measures

`crawl.ts` loads each page and runs `collect.ts` in it. `collect.ts` walks
every visible element and reads its computed style:

- **Type:** the font size and line height of each element with its own text.
- **Spacing:** every non-zero margin, padding and flex or grid gap in px.
- **Colour:** how many distinct text, background and border colours it
  draws. Only the count is kept, never a colour.
- **Radii:** each non-zero corner radius.
- **Controls and rows:** the height of each control (`button`, inputs,
  `role=button|tab|switch|…`) and of each row (`li`, `tr`,
  `role=row|option|treeitem|…`).
- **Focus:** the crawl presses Tab 20 times and records how each focused
  element shows its focus: an outline with its width and offset, a
  box-shadow, or nothing.
- **Two AIM-style metrics.** These are computed here, not by the
  [AIM](https://github.com/aalto-ui/aim) service. *Complexity* is the PNG
  size of the viewport screenshot per pixel, which is AIM's own
  file-size metric of visual clutter. *Alignment* is the number of distinct
  left edges per 100 boxes (text, controls, images): a DOM stand-in for
  AIM's grid quality, where lower means more aligned. AIM's
  segmentation-based grid and feature-congestion metrics were not run (see
  "Limits"). Treat both as trends, not gates.

[Project Wallace](https://github.com/projectwallace/css-design-tokens)
(`@projectwallace/css-design-tokens` 0.11.13) reads the CSS each page loaded
and extracts DTCG tokens. The results keep only the count per token type.
This counts the CSS as *authored*, which includes every rule a site ships,
whether this page uses it or not.

Run it by hand, never in CI. In `web/baseline`: `bun install`, then
`bun crawl.ts` (`--only <site>` runs one site). It needs Playwright's
Chromium (`just client-browsers`) and the client's dependencies, because it
builds flock's gallery and fixture workbench first. To measure a logged-in
app on an occasional manual run, pass `--storage-state <file>`, a Playwright
storage state from the owner's own test account. Do not commit those
results. `web/client/e2e/baseline.test.ts` checks the measurements on a page
whose every value is known, and checks that the results contain counts and
no colour. It runs in `just client-check`.

## What was measured

| Site | Pages (public only) |
| --- | --- |
| flock | the gallery; the fixture workbench |
| iCloud | `www.icloud.com` (the signed-out start page) |
| Linear | home, docs, sign-in |
| Vercel | docs, sign-in |
| GitHub | this repository's code and issues pages, signed out |
| Primer | the Button component page |
| Stripe | docs, dashboard sign-in |

The issue named the iCloud web apps and the Linear, Vercel and Stripe
dashboards. Every one of them requires a sign-in, so this run measured
their public pages instead. Docs pages and GitHub's signed-out repository
pages are the closest to an app: dense chrome, sidebars, lists and controls.
Sign-in pages show each product's form controls. The marketing home pages
(iCloud, Linear) are listed in the results but left out of the medians,
because they are posters, not tools.

Only GitHub follows `prefers-color-scheme` on these pages. The others render
the same in both modes, so their dark rows repeat their light ones. Per-mode
colour counts are only meaningful for flock and GitHub.

## Results

The **app pages** are Linear docs, Vercel docs, GitHub repo and issues,
Primer and Stripe docs. "Distinct" means distinct values on one page. The
light mode is shown.

| Page | Font sizes | Line heights | Spacing values | On 4 px | Colours | Radii | Control heights (most used) | Row heights (most used) | Focus styles |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| flock gallery | 4 | 2 | 5 | 100 % | 12 | 2 | 28 | 28 | 1 |
| flock workbench | 6 | 8 | 12 | 0 % | 10 | 4 | 44, 53 | (none) | 1 |
| Linear docs | 6 | 6 | 28 | 71 % | 8 | 2 | 36, 32 | 36 | 3 |
| Vercel docs | 9 | 12 | 22 | 48 % | 15 | 9 | 32, 51 | 28, 36 | 3 (one is none) |
| GitHub repo | 8 | 13 | 20 | 45 % | 26 | 7 | 28, 32 | 37, 41 | 2 |
| GitHub issues | 7 | 10 | 13 | 90 % | 24 | 6 | 18, 22, 32, 28 | 89, 32 | 2 |
| Primer button | 11 | 18 | 18 | 90 % | 26 | 5 | 28, 32, 40 | 36 | 3 |
| Stripe docs | 7 | 9 | 19 | 85 % | 24 | 7 | 49, 24 | 36, 26 | 3 (one is none) |
| **App median** | **7.5** | **11** | **19.5** | **78 %** | **24** | **6.5** | | | **3** |
| Linear sign-in | 2 | 1 | 7 | 50 % | 5 | 1 | 44 | | 3 |
| Vercel sign-in | 4 | 4 | 9 | 46 % | 6 | 3 | 40 | | 3 (one is none) |
| Stripe sign-in | 5 | 6 | 16 | 84 % | 12 | 4 | 40, 44 | | 3 (one is none) |

The most used values, beyond the counts:

- **Type:** the body size on app pages is 12–14 px. GitHub's 13.6 and the
  workbench's 11.38, 12.25, 10.5 and 9.63 px are rem fractions, and so is
  the workbench's 14 px body.
- **Radii:** each app page has one dominant radius, 6 px on GitHub, Primer
  and Vercel, 8 px on Linear and 4 px on Stripe. Every one of them also
  draws pills or circles (`9999px`, `50%`) for avatars, counts and tags.
- **Focus:** every exemplar draws an outline of 1–2 px, at an offset of −2
  to 4 px. Vercel and Stripe leave a few focused elements with no visible
  ring, which flock's keyboard rule already forbids.

| Page | Complexity (PNG bytes per px) | Alignment (left edges per 100 boxes) |
| --- | --- | --- |
| flock gallery | 0.039 | 48.1 (a specimen sheet, not a screen) |
| flock workbench | 0.127 (dark 0.123) | 17.2 |
| Linear docs | 0.083 | 19.1 |
| Primer button | 0.090 | 17.6 |
| Vercel docs | 0.108 | 19.5 |
| Stripe docs | 0.144 | 33.0 |
| GitHub issues | 0.158 | 36.9 |
| GitHub repo | 0.161 | 32.0 |

**Project Wallace, authored CSS** (tokens per type, as Wallace extracts them):

| Page | Colours | Font sizes | Line heights | Radii | Shadows |
| --- | --- | --- | --- | --- | --- |
| flock workbench | 62 | 11 | 4 | 6 | 1 |
| Linear docs | 267 | 49 | 42 | 47 | 45 |
| Vercel docs | 308 | 47 | 37 | 39 | 37 |
| Stripe docs | 309 | 44 | 35 | 40 | 118 |
| Primer button | 899 | 58 | 61 | 51 | 58 |
| GitHub repo | 1159 | 108 | 95 | 81 | 150 |

## Comparison

- **flock's new tokens are stricter than every exemplar, and that is
  right.** On its gallery, flock renders 4 font sizes, 5 spacing values,
  2 radii, one control height and one focus style. Every exemplar app page
  renders more of each. No exemplar keeps all of its spacing on a 4 px grid:
  the best reach 90 % (Primer, GitHub issues), and Vercel reaches 48 %. The
  authored CSS tells the same story: flock ships 4 to 19 times fewer
  colour, type and radius tokens.
- **The workbench is the outlier, and the reason is known.** Its legacy
  stylesheets, sized in rem on a 14 px root (#2913), give 0 % of spacing on
  the grid, fractional type sizes (one of them, 9.63 px, is under the 10 px
  minimum), 8 line heights and 44 px desktop controls. That is the list
  `conformance-allow.ts` already holds against #2913. Nothing new is
  needed, only that work.
- **28 px controls are the exemplars' norm.** GitHub's repository page, Primer and Linear's
  marketing page use 28 px controls more than any other height, which
  matches `--size-control`. Larger controls (32–40 px) appear as a second
  size on Primer and GitHub, and on sign-in forms (40–44 px).
- **flock has two focus rings.** The gallery draws a 2 px outline at a 1 px
  offset; the workbench's legacy `styles.css` draws it at 2 px. Each page
  has a single style, but the app has two.
- **Density is in the exemplars' range.** The workbench's complexity, 0.127,
  falls between Vercel docs (0.108) and GitHub (0.16). Its alignment, 17.2,
  is as good as the best exemplar (Primer, 17.6).

## Proposed budgets

Each budget is a test for `e2e/conformance.test.ts` (#2896), run per page
and mode like the existing rules, with a seed that breaks it. The numbers
come from the evidence above. Where flock already does better than every
exemplar, the budget holds flock's own scale rather than the exemplars'
medians, so the budget can only tighten.

| Budget | Limit | Evidence | flock today |
| --- | --- | --- | --- |
| **Type on the scale.** Every computed font size is a `--font-size-*` value. | the 7 steps | Exemplar pages use 6–11 sizes; rem fractions are where the extra ones come from. | gallery passes; workbench fails (rem, #2913) |
| **Font sizes per screen** | ≤ 6 | App median 7.5; Linear docs 6, Stripe 7. | gallery 4, workbench 6 |
| **One line height per size.** Each font size renders with one line height. | 1 per size | Exemplars use 6–18 line heights for 6–11 sizes, so their pairs drift. | gallery 2 for 4 sizes (passes); workbench 8 for 6 |
| **Spacing values per screen** | ≤ 9 (the scale) plus the 1 px hairline | App median 19.5 distinct values, at 45–90 % on 4 px. | gallery 5, workbench 12 |
| **Colours per screen and mode** | ≤ 16 | App median 24 (GitHub 24–30); Linear docs 8, Vercel 15. | gallery 12–13, workbench 10–11 |
| **Radii per screen** | ≤ 4: three sizes plus a pill | App median 6.5; each page has one dominant radius and a pill. | gallery 2, workbench 4 |
| **Control heights per desktop screen** | ≤ 3 (20, 28, and one larger size if one is added) | Primer 28/32/40, GitHub 28/32. | gallery 1, workbench 5 (44 px legacy) |
| **One focus style across the app** | 1 for all pages | Exemplars mix 2–3 styles on one page, and some have no ring. | 2 (gallery 1 px offset, workbench 2 px) |
| **Complexity, a trend** | workbench ≤ 0.16 | The densest exemplar measured (GitHub). | 0.127 |
| **Alignment, a trend** | workbench ≤ 20 | Linear, Primer and Vercel docs, 17.6–19.5. | 17.2 |

The two trends are advisory. Fonts differ between macOS and the Linux CI
runner, and so do PNG sizes, so a hard gate on them would flake. Record them
per run instead and flag a jump.

## Token calibration (#2895)

- **Keep:** the type scale (10–22 px, 13 px body), the 4 px spacing scale,
  `--radius-small` (6 px, the dominant radius on GitHub, Primer and Vercel),
  and `--size-control` (28 px, the exemplars' most used control height).
- **Add when first used:** a pill radius (`--radius-full`). Every exemplar
  draws pills or circles, and the workbench's count badge already does it
  with a raw `999px` in the legacy CSS. Add it with the badge's redesign, by
  the rule in `web/DESIGN.md` that a token arrives with its first use.
- **Add: line-height tokens paired with the type scale.** There are none
  today, so line height falls to the browser and the legacy CSS (8 values
  on the workbench). Pairing each size with one line height, taken from the
  HIG's macOS type table, makes "one line height per size" a token property.
- **Align the focus ring:** one outline width and offset as tokens, used by
  the gallery and the workbench alike.
- **Hold:** a larger control size (32 px) only when a screen needs a second
  size. The exemplars use one, but flock has no such screen yet.

## Limits

- **Public pages only.** The app screens the issue named are behind sign-in.
  The docs and GitHub pages stand in for them. A manual run with
  `--storage-state` measures the apps themselves.
- **One snapshot.** Each page was measured once, signed out, at one
  viewport. Sites change, so the date matters more than the decimals.
- **Most exemplar pages ignore `prefers-color-scheme`** when signed out, so
  their dark rows say nothing about their dark themes.
- **The AIM numbers are AIM-style, not AIM's.** The AIM service (a Python
  backend with image segmentation) was not run. Complexity uses AIM's
  file-size metric directly. Alignment is a DOM stand-in for AIM's grid
  quality. AIM's feature-congestion clutter has no stand-in.
- **Wallace counts authored CSS.** This includes rules a page never uses,
  so it measures the size of a design system rather than the discipline of a
  screen. flock's colour count includes all four modes' token definitions.
- **The walk stops at 20 000 elements.** No page reached that limit; Linear's
  home page, the largest, had 2 990.
- **Wallace is EUPL-1.2.** It lives in this tool's own `package.json`, never
  in the client's dependencies or build.
