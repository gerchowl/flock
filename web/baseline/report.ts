/**
 * The report's tables (#2899), rendered from `results/baseline.json`, so the
 * README cannot drift from the data. `bun report.ts` rewrites each region
 * between `<!-- table:<name> -->` and `<!-- /table -->` in `README.md`;
 * `web/client/e2e/baseline.test.ts` fails while a region differs from what
 * `render` makes of the committed results.
 */
import type { Distribution, Summary } from "./collect";

type Result = { readonly site: string; readonly page: string; readonly mode: string; readonly summary?: Summary; readonly wallace?: Readonly<Record<string, number>> };

/** The pages the report shows, in order, with their labels. App pages make up the medians. */
const ROWS: readonly { key: string; label: string; app?: true }[] = [
  { key: "flock/gallery", label: "flock gallery (specimen sheet)" },
  { key: "flock/workbench", label: "flock workbench" },
  { key: "linear/docs", label: "Linear docs", app: true },
  { key: "vercel/docs", label: "Vercel docs", app: true },
  { key: "github/repo", label: "GitHub repo", app: true },
  { key: "github/issues", label: "GitHub issues", app: true },
  { key: "primer/button", label: "Primer button", app: true },
  { key: "stripe/docs", label: "Stripe docs", app: true },
  { key: "linear/login", label: "Linear sign-in" },
  { key: "vercel/login", label: "Vercel sign-in" },
  { key: "stripe/login", label: "Stripe sign-in" },
  { key: "icloud/home", label: "iCloud home (marketing)" },
  { key: "linear/home", label: "Linear home (marketing)" },
];

function median(values: readonly number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 1 ? sorted[middle]! : (sorted[middle - 1]! + sorted[middle]!) / 2;
}

/** How many distinct values, then the two most used with their counts: `2: 36 (11), 32 (2)`. */
const top = (distribution: Distribution): string =>
  distribution.distinct === 0 ? "none" : `${distribution.distinct}: ${distribution.values.slice(0, 2).map(([value, count]) => `${value} (${count})`).join(", ")}`;
const percent = (share: number): string => `${Math.round(share * 100)} %`;
const table = (head: readonly string[], rows: readonly (readonly (string | number)[])[]): string =>
  [`| ${head.join(" | ")} |`, `| ${head.map(() => "---").join(" | ")} |`, ...rows.map((row) => `| ${row.join(" | ")} |`)].join("\n");

/** Every table of the report, by region name. */
export function render(results: readonly Result[]): Record<string, string> {
  const find = (key: string, mode: string): Result => {
    const found = results.find((result) => `${result.site}/${result.page}` === key && result.mode === mode);
    if (found?.summary === undefined || found.wallace === undefined) throw new Error(`no measurement of ${key} in ${mode}`);
    return found;
  };
  const light = (key: string) => find(key, "light").summary!;
  const dark = (key: string) => find(key, "dark").summary!;
  const apps = ROWS.filter((row) => row.app).map((row) => light(row.key));
  const focusStyles = (key: string) => new Set([...light(key).focus.values, ...dark(key).focus.values].map(([style]) => style));

  const shape = table(
    ["Page", "Font sizes", "Line heights", "Spacing values", "On 4 px", "Colours (light / dark)", "Radii", "Control heights (distinct: most used)", "Row heights (distinct: most used)"],
    [
      ...ROWS.map(({ key, label }) => {
        const summary = light(key);
        return [label, summary.fontSizes.distinct, summary.lineHeights.distinct, summary.spacing.distinct, percent(summary.spacing.onGrid4), `${summary.colours.total} / ${dark(key).colours.total}`, summary.radii.distinct, top(summary.controlHeights), top(summary.rowHeights)];
      }),
      [
        "**App median**",
        median(apps.map((each) => each.fontSizes.distinct)),
        median(apps.map((each) => each.lineHeights.distinct)),
        median(apps.map((each) => each.spacing.distinct)),
        percent(median(apps.map((each) => each.spacing.onGrid4))),
        median(apps.map((each) => each.colours.total)),
        median(apps.map((each) => each.radii.distinct)),
        "",
        "",
      ],
    ],
  );

  const focus = table(
    ["Page", "Styles (light / dark)", "Outline widths", "Outline offsets", "A box-shadow ring", "Some focus not shown"],
    ROWS.map(({ key, label }) => {
      const styles = [...focusStyles(key)];
      const outlines = styles.map((style) => /^outline (\d+)px offset (-?\d+)px$/.exec(style)).filter((match) => match !== null);
      const sorted = (values: number[]) => [...new Set(values)].sort((a, b) => a - b).map((value) => `${value} px`).join(", ") || "none";
      return [
        label,
        `${light(key).focus.distinct} / ${dark(key).focus.distinct}`,
        sorted(outlines.map((match) => Number(match[1]))),
        sorted(outlines.map((match) => Number(match[2]))),
        styles.includes("box-shadow") ? "yes" : "no",
        styles.includes("none") ? "yes" : "no",
      ];
    }),
  );

  const metrics = table(
    ["Page", "Complexity, light (PNG bytes per px)", "Complexity, dark", "Alignment (left edges per 100 boxes)"],
    ROWS.map(({ key, label }) => [label, light(key).complexity, dark(key).complexity, light(key).alignment]),
  );

  const wallace = table(
    ["Page", "Colours", "Font sizes", "Line heights", "Radii", "Shadows"],
    ROWS.filter(({ key, app }) => app === true || key === "flock/workbench").map(({ key, label }) => {
      const counts = find(key, "light").wallace!;
      return [label, counts.color ?? 0, counts.font_size ?? 0, counts.line_height ?? 0, counts.radius ?? 0, counts.box_shadow ?? 0];
    }),
  );

  return { shape, focus, metrics, wallace };
}

/** `readme` with every table region replaced by its rendering. */
export function fill(readme: string, tables: Readonly<Record<string, string>>): string {
  // A region runs from its opening marker to the next closing one, never past it, even when empty.
  return readme.replace(/(<!-- table:([a-z]+) -->\n)(?:(?!<!-- \/table -->)[\s\S])*(<!-- \/table -->)/g, (_, open: string, name: string, close: string) => {
    const rendered = tables[name];
    if (rendered === undefined) throw new Error(`README names an unknown table: ${name}`);
    return `${open}${rendered}\n${close}`;
  });
}

if (import.meta.main) {
  const dir = import.meta.dir;
  const { results } = (await Bun.file(`${dir}/results/baseline.json`).json()) as { results: Result[] };
  const readme = await Bun.file(`${dir}/README.md`).text();
  await Bun.write(`${dir}/README.md`, fill(readme, render(results)));
}
