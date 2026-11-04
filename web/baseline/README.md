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
whose every value is known, checks that the results contain counts and no
colour, and checks that this report's tables match the results (`bun
report.ts` rewrites them). It runs in `just client-check`.

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

Four of these pages follow `prefers-color-scheme` when signed out: GitHub's
repository and issues pages, iCloud's start page and Linear's sign-in page.
Their screenshots change between modes, and so does the complexity metric.
A re-check outside the committed run confirmed that iCloud's body and Linear
sign-in's whole page turn dark. Their colour *counts* can still match across
modes, because a dark theme may use as many colours as a light one. The
other exemplar pages measured the same in both modes, down to the PNG size
of their screenshots, so signed out they do not follow the scheme, and
their dark rows say nothing about a dark theme.

## Results

The **app pages** are Linear docs, Vercel docs, GitHub repo and issues,
Primer and Stripe docs, and the medians are theirs. Every number counts the
distinct values on one page, in light mode unless a column says otherwise.
The flock gallery is a specimen sheet (every primitive and control state
side by side), not a screen, so compare a screen's counts with it only as a
lower bound. `bun report.ts` renders these tables from `results/baseline.json`,
and `web/client/e2e/baseline.test.ts` fails when they differ.

<!-- table:shape -->
| Page | Font sizes | Line heights | Spacing values | On 4 px | Colours (light / dark) | Radii | Control heights (distinct: most used) | Row heights (distinct: most used) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| flock gallery (specimen sheet) | 4 | 2 | 5 | 100 % | 12 / 13 | 2 | 1: 28 (9) | 1: 28 (3) |
| flock workbench | 6 | 8 | 12 | 0 % | 10 / 11 | 4 | 5: 44 (23), 53 (8) | none |
| Linear docs | 6 | 6 | 28 | 71 % | 8 / 8 | 2 | 2: 36 (11), 32 (2) | 2: 36 (11), 24 (1) |
| Vercel docs | 9 | 12 | 22 | 48 % | 15 / 15 | 9 | 6: 32 (11), 51 (3) | 6: 28 (80), 36 (70) |
| GitHub repo | 8 | 13 | 20 | 45 % | 26 / 28 | 7 | 5: 28 (19), 32 (4) | 9: 37 (43), 41 (37) |
| GitHub issues | 7 | 10 | 13 | 90 % | 24 / 30 | 6 | 6: 18 (27), 22 (25) | 9: 89 (20), 32 (15) |
| Primer button | 11 | 18 | 18 | 90 % | 26 / 26 | 5 | 9: 28 (53), 32 (22) | 23: 36 (151), 94 (9) |
| Stripe docs | 7 | 9 | 19 | 85 % | 24 / 24 | 7 | 8: 49 (7), 24 (3) | 4: 36 (9), 26 (7) |
| Linear sign-in | 2 | 1 | 7 | 50 % | 5 / 5 | 1 | 1: 44 (4) | none |
| Vercel sign-in | 4 | 4 | 9 | 46 % | 6 / 6 | 3 | 2: 40 (8), 20 (1) | none |
| Stripe sign-in | 5 | 6 | 16 | 84 % | 12 / 12 | 4 | 4: 40 (3), 44 (3) | none |
| iCloud home (marketing) | 6 | 6 | 16 | 48 % | 8 / 8 | 3 | 2: 36 (1), 44 (1) | none |
| Linear home (marketing) | 14 | 17 | 54 | 65 % | 51 / 51 | 15 | 8: 28 (31), 29 (20) | 9: 28 (33), 32 (10) |
| **App median** | 7.5 | 11 | 19.5 | 78 % | 24 | 6.5 |  |  |
<!-- /table -->

**Focus**, over the 20 Tab stops in both modes. "Some focus not shown"
means at least one focused element drew neither an outline nor a box-shadow:

<!-- table:focus -->
| Page | Styles (light / dark) | Outline widths | Outline offsets | A box-shadow ring | Some focus not shown |
| --- | --- | --- | --- | --- | --- |
| flock gallery (specimen sheet) | 1 / 1 | 2 px | 1 px | no | no |
| flock workbench | 1 / 1 | 2 px | 2 px | no | no |
| Linear docs | 3 / 3 | 1 px | -2 px, -1 px, 2 px | no | no |
| Vercel docs | 3 / 3 | 2 px | 4 px | yes | yes |
| GitHub repo | 2 / 2 | 2 px | -2 px, 0 px | no | no |
| GitHub issues | 2 / 2 | 2 px | -2 px, 0 px | no | no |
| Primer button | 4 / 4 | 1 px, 2 px | -2 px, 0 px, 1 px | no | yes |
| Stripe docs | 4 / 4 | 1 px, 2 px, 3 px | 0 px, 1 px | yes | yes |
| Linear sign-in | 4 / 4 | 1 px | -1 px, 0 px, 1 px, 2 px | no | no |
| Vercel sign-in | 3 / 3 | 2 px | 4 px | yes | yes |
| Stripe sign-in | 5 / 5 | 1 px | 0 px, 1 px, 2 px | yes | yes |
| iCloud home (marketing) | 2 / 2 | 1 px | 1 px | yes | no |
| Linear home (marketing) | 2 / 2 | 1 px | -2 px, 2 px | no | no |
<!-- /table -->

**AIM-style metrics.** Complexity is lower when calmer, and alignment is
lower when more aligned:

<!-- table:metrics -->
| Page | Complexity, light (PNG bytes per px) | Complexity, dark | Alignment (left edges per 100 boxes) |
| --- | --- | --- | --- |
| flock gallery (specimen sheet) | 0.039 | 0.039 | 48.1 |
| flock workbench | 0.127 | 0.123 | 17.2 |
| Linear docs | 0.083 | 0.083 | 19.1 |
| Vercel docs | 0.108 | 0.108 | 19.5 |
| GitHub repo | 0.161 | 0.163 | 32 |
| GitHub issues | 0.158 | 0.159 | 36.9 |
| Primer button | 0.09 | 0.09 | 17.6 |
| Stripe docs | 0.144 | 0.144 | 33 |
| Linear sign-in | 0.027 | 0.035 | 60 |
| Vercel sign-in | 0.042 | 0.042 | 65.6 |
| Stripe sign-in | 0.443 | 0.443 | 65.6 |
| iCloud home (marketing) | 0.075 | 0.102 | 72.2 |
| Linear home (marketing) | 0.301 | 0.301 | 45.6 |
<!-- /table -->

**Project Wallace, authored CSS** (tokens per type, as Wallace extracts
them from everything the page loaded):

<!-- table:wallace -->
| Page | Colours | Font sizes | Line heights | Radii | Shadows |
| --- | --- | --- | --- | --- | --- |
| flock workbench | 62 | 11 | 4 | 6 | 1 |
| Linear docs | 267 | 49 | 42 | 47 | 45 |
| Vercel docs | 308 | 47 | 37 | 39 | 37 |
| GitHub repo | 1159 | 108 | 95 | 81 | 150 |
| GitHub issues | 1160 | 115 | 100 | 86 | 155 |
| Primer button | 899 | 58 | 61 | 51 | 58 |
| Stripe docs | 309 | 44 | 35 | 40 | 118 |
<!-- /table -->

## Comparison

- **flock's tokens are tighter than the exemplars, but not on every
  measure.** The gallery renders 4 font sizes, 5 spacing values, one control
  height and one focus style, and every exemplar app page renders more of
  each. It renders 2 radii, which is as few as Linear docs, the fewest of
  the app pages. All of its spacing is on the 4 px grid, and the best
  exemplars reach 90 % (Primer, GitHub issues). In authored CSS, flock ships
  4 to 19 times fewer colour, type and radius tokens than any exemplar app
  page. The gallery is a specimen sheet, so these figures are a lower bound
  for a flock screen, not a measured screen.
- **The workbench is the outlier, and the reason is known.** Its legacy
  stylesheets, sized in rem on a 14 px root (#2913), put none of its spacing
  on the grid, give it fractional type sizes (one, the needs-you count at
  9.63 px, is under the 10 px minimum), 8 line heights for 6 sizes, and
  44 px desktop controls. That is what `conformance-allow.ts` already holds
  against #2913, so nothing new is needed, only that work.
- **The exemplars have no common control height.** Among the app pages,
  28 px is the most used height only on GitHub repo and Primer. Linear docs
  mostly use 36 px, Vercel docs 32 px, GitHub issues 18 px and Stripe docs
  49 px. Sign-in forms use 40–44 px. The data neither confirms nor refutes
  28 px. flock keeps it because the HIG makes 28 px the regular macOS
  control size (`web/DESIGN.md`, Buttons), and the data shows it sits inside
  the exemplars' 18–49 px range.
- **Every exemplar page mixes focus styles, and several leave some focus
  unshown.** App pages draw 2 to 4 styles each per mode, with outlines of 1–3 px
  (Stripe docs' 3 px ring appears in dark mode only) at offsets of −2 to
  4 px. Vercel docs, Primer and Stripe docs, and the Vercel and Stripe
  sign-in pages, each left at least one focused element with no ring, which
  flock's keyboard rule forbids. flock draws one style per page, but two
  across the app: a 1 px offset on the gallery and 2 px on the workbench's
  legacy `styles.css`.
- **Each app page has one dominant radius**: 6 px on GitHub, Primer and
  Vercel docs, 8 px on Linear docs and 4 px on Stripe docs. All but Stripe
  docs also draw pills or circles (`9999px`, `50%`) for avatars, counts and
  tags.
- **Density is in the exemplars' range.** The workbench's complexity sits
  between Vercel docs and Stripe docs, and its alignment is as good as the
  best app page's (Primer).

## Proposed budgets

Each budget is a test for `e2e/conformance.test.ts` (#2896), run per page
and mode like the existing rules, with a seed that breaks it. "Basis" says
where each limit comes from. Where it reads **design choice**, the data does
not set the number: the exemplars are looser, and the limit holds flock's own
scale or the HIG instead, so it can only tighten.

| Budget | Limit | Basis | flock today |
| --- | --- | --- | --- |
| **Type on the scale.** Every computed font size is a `--font-size-*` value. | the 7 steps | Design choice (the HIG's macOS scale). The data shows where extra sizes come from: rem fractions (GitHub's 13.6 px, the workbench's). | gallery passes; workbench fails (#2913) |
| **Font sizes per screen** | ≤ 6 | Design choice. App pages use 6–11 (median 7.5); only Linear docs is at 6. | gallery 4, workbench 6 |
| **One line height per size.** Each font size renders with one line height. | 1 per size | Design choice. Every app page but Linear docs renders more line heights than sizes. | gallery 2 for 4 sizes; workbench 8 for 6 |
| **Spacing values per screen** | ≤ 9 (the scale) plus the 1 px hairline | Design choice (the token scale). App pages use 13–28 values (median 19.5). | gallery 5, workbench 12 |
| **Colours per screen and mode** | ≤ 16 | Design choice with headroom: flock renders 10–13 today. App pages use 8–26 in light mode and up to 30 in dark (light median 24). | gallery 12–13, workbench 10–11 |
| **Radii per screen** | ≤ 4: three sizes plus a pill | Design choice (three tokens and a pill). App pages use 2–9 (median 6.5), with one dominant radius and, on five of six, a pill. | gallery 2, workbench 4 |
| **Control heights per desktop screen** | ≤ 3 (20, 28 and one larger size if one is added) | Design choice (the HIG's small and regular sizes). App pages use 2–9 heights, and no height is common to them. | gallery 1, workbench 5 |
| **One focus style across the app** | 1 for all pages | Design choice (the HIG's one focus ring). Every exemplar page mixes 2–5 styles. | 2 (gallery 1 px offset, workbench 2 px) |
| **Complexity, a trend** | workbench ≤ 0.161 | The data: the densest app page measured (GitHub repo). | 0.127 |
| **Alignment, a trend** | workbench ≤ 20 | The data: the three most aligned app pages (Primer, Linear docs, Vercel docs) are at 17.6–19.5. | 17.2 |

The two trends are advisory. Fonts differ between macOS and the Linux CI
runner, and so do PNG sizes, so a hard gate on them would flake. Record them
per run instead and flag a jump.

## Token calibration (#2895)

- **Keep:** the type scale (10–22 px, 13 px body) and the 4 px spacing
  scale. Keep `--radius-small` (6 px), which is the dominant radius on four
  of the six app pages (both GitHub pages, Primer and Vercel docs). Keep `--size-control` (28 px) for the HIG's reason,
  not the exemplars': they share no control height.
- **Add when first used:** a pill radius (`--radius-full`). Five of the six
  app pages draw pills or circles, and the workbench's count badge already
  does it with a raw `999px` in the legacy CSS. Add it with the badge's
  redesign: under the rule in `web/DESIGN.md`, a token arrives with its
  first use.
- **Add: line-height tokens paired with the type scale.** There are none
  today, so line height is left to the browser and the legacy CSS (8 values
  on the workbench). Pairing each size with one line height, taken from the
  HIG's macOS type table, makes "one line height per size" a property of the
  tokens.
- **Align the focus ring:** one outline width and offset as tokens, used by
  the gallery and the workbench alike.
- **Hold:** add a larger control size (32 px) only when a screen needs a
  second size. Primer and GitHub use one, but flock has no such screen yet.

## Limits

- **Public pages only.** The app screens the issue named are behind sign-in.
  The docs and GitHub pages stand in for them. A manual run with
  `--storage-state` measures the apps themselves.
- **One snapshot.** Each page was measured once, signed out, at one
  viewport. Sites change, so the date matters more than the decimals.
- **Most exemplar pages ignore `prefers-color-scheme`** when signed out (see
  above), so for them a dark row repeats the light one.
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
