/**
 * The exemplar baseline's measurements (#2899): what one rendered page uses,
 * read from the browser's computed styles. `collect` runs inside the page
 * (it is serialised by `page.evaluate`, so it closes over nothing), and
 * `summarise` turns its samples into the distributions the report compares.
 *
 * Colours are counted and never kept: a summary holds how many distinct
 * colours a page renders, not which ones (the issue forbids copying a
 * palette). Everything else is a number with its count.
 */

/** What `collect` reads from one page. */
export type Samples = {
  readonly elements: number;
  readonly fontSizes: number[];
  readonly lineHeights: (number | "normal")[];
  readonly spacing: number[];
  readonly textColours: string[];
  readonly fillColours: string[];
  readonly borderColours: string[];
  readonly radii: string[];
  readonly controlHeights: number[];
  readonly rowHeights: number[];
  readonly leftEdges: number[];
};

/** Reads every visible element of the page; `limit` bounds the walk on very large pages. */
export function collect(limit: number): Samples {
  const px = (value: string): number => Math.round(Number.parseFloat(value) * 100) / 100;
  const out = {
    elements: 0,
    fontSizes: [] as number[],
    lineHeights: [] as (number | "normal")[],
    spacing: [] as number[],
    textColours: [] as string[],
    fillColours: [] as string[],
    borderColours: [] as string[],
    radii: [] as string[],
    controlHeights: [] as number[],
    rowHeights: [] as number[],
    leftEdges: [] as number[],
  };
  const CONTROL = "button, input:not([type=hidden]), select, textarea, [role=button], [role=tab], [role=switch], [role=checkbox], [role=combobox], [role=searchbox]";
  const ROW = "li, tr, [role=row], [role=option], [role=listitem], [role=treeitem], [role=menuitem]";
  const transparent = (colour: string) => colour === "transparent" || /^rgba\(.*,\s*0\)$/.test(colour);
  for (const element of [document.body, ...document.body.querySelectorAll("*")]) {
    if (out.elements >= limit) break;
    const style = getComputedStyle(element);
    const box = element.getBoundingClientRect();
    if (box.width === 0 || box.height === 0 || style.visibility === "hidden" || style.display === "none" || style.opacity === "0") continue;
    out.elements += 1;
    const text = [...element.childNodes].some((node) => node.nodeType === Node.TEXT_NODE && node.textContent!.trim() !== "");
    if (text) {
      out.fontSizes.push(px(style.fontSize));
      out.lineHeights.push(style.lineHeight === "normal" ? "normal" : px(style.lineHeight));
      out.textColours.push(style.color);
    }
    for (const side of ["Top", "Right", "Bottom", "Left"] as const) {
      for (const value of [style.getPropertyValue(`margin-${side.toLowerCase()}`), style.getPropertyValue(`padding-${side.toLowerCase()}`)]) {
        if (value.endsWith("px") && px(value) > 0) out.spacing.push(px(value));
      }
      const width = style.getPropertyValue(`border-${side.toLowerCase()}-width`);
      if (px(width) > 0 && style.getPropertyValue(`border-${side.toLowerCase()}-style`) !== "none") {
        const colour = style.getPropertyValue(`border-${side.toLowerCase()}-color`);
        if (!transparent(colour)) out.borderColours.push(colour);
      }
    }
    if (/flex|grid/.test(style.display)) {
      for (const value of [style.rowGap, style.columnGap]) if (value.endsWith("px") && px(value) > 0) out.spacing.push(px(value));
    }
    if (!transparent(style.backgroundColor)) out.fillColours.push(style.backgroundColor);
    for (const corner of ["top-left", "top-right", "bottom-right", "bottom-left"]) {
      const radius = style.getPropertyValue(`border-${corner}-radius`);
      if (radius !== "0px" && radius !== "") out.radii.push(radius);
    }
    if (element.matches(CONTROL)) out.controlHeights.push(Math.round(box.height));
    else if (element.matches(ROW)) out.rowHeights.push(Math.round(box.height));
    if (text || element.matches(CONTROL) || element.matches("img, svg")) out.leftEdges.push(Math.round(box.left + scrollX));
  }
  return out;
}

/** How a focused control shows its focus, as `collect`'s caller reads it after a Tab. */
export function focusStyle(): string | null {
  const element = document.activeElement;
  if (element === null || element === document.body) return null;
  const style = getComputedStyle(element);
  if (style.outlineStyle !== "none" && Number.parseFloat(style.outlineWidth) > 0) {
    return `outline ${Math.round(Number.parseFloat(style.outlineWidth))}px offset ${Math.round(Number.parseFloat(style.outlineOffset))}px`;
  }
  if (style.boxShadow !== "none") return "box-shadow";
  return "none";
}

/** A distribution: how many distinct values, and each value with its count, most used first. */
export type Distribution = { readonly distinct: number; readonly values: readonly (readonly [string, number])[] };

function distribution(values: readonly (number | string)[]): Distribution {
  const counts = new Map<string, number>();
  for (const value of values) counts.set(String(value), (counts.get(String(value)) ?? 0) + 1);
  const sorted = [...counts].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0], "en", { numeric: true }));
  return { distinct: sorted.length, values: sorted };
}

/** The share of `values` that are whole multiples of `step`, to two decimals; 1 when there are none. */
export function onGrid(values: readonly number[], step: number): number {
  if (values.length === 0) return 1;
  return Math.round((values.filter((value) => value % step === 0).length / values.length) * 100) / 100;
}

/** One page in one mode, as the report reads it. */
export type Summary = {
  readonly elements: number;
  readonly fontSizes: Distribution;
  readonly lineHeights: Distribution;
  readonly spacing: Distribution & { readonly onGrid4: number; readonly onGrid8: number };
  readonly colours: { readonly text: number; readonly fill: number; readonly border: number; readonly total: number };
  readonly radii: Distribution;
  readonly controlHeights: Distribution;
  readonly rowHeights: Distribution;
  readonly focus: Distribution;
  /** AIM-style, see the report: PNG bytes per viewport pixel (visual complexity) and distinct left edges per 100 aligned boxes. */
  readonly complexity: number;
  readonly alignment: number;
};

/** Summarises one page's samples, its focus styles and its viewport screenshot's PNG size. */
export function summarise(samples: Samples, focus: readonly (string | null)[], png: { bytes: number; pixels: number }): Summary {
  const colours = { text: new Set(samples.textColours).size, fill: new Set(samples.fillColours).size, border: new Set(samples.borderColours).size };
  const total = new Set([...samples.textColours, ...samples.fillColours, ...samples.borderColours]).size;
  const edges = new Set(samples.leftEdges).size;
  return {
    elements: samples.elements,
    fontSizes: distribution(samples.fontSizes),
    lineHeights: distribution(samples.lineHeights),
    spacing: { ...distribution(samples.spacing), onGrid4: onGrid(samples.spacing, 4), onGrid8: onGrid(samples.spacing, 8) },
    colours: { ...colours, total },
    radii: distribution(samples.radii),
    controlHeights: distribution(samples.controlHeights),
    rowHeights: distribution(samples.rowHeights),
    focus: distribution(focus.filter((each): each is string => each !== null)),
    complexity: Math.round((png.bytes / png.pixels) * 1000) / 1000,
    alignment: samples.leftEdges.length === 0 ? 0 : Math.round((edges / samples.leftEdges.length) * 1000) / 10,
  };
}
