/**
 * The in-page half of the design-conformance suite (#2896, web/DESIGN.md
 * "Conformance"): {@link installChecks} runs inside the page and defines
 * `globalThis.__hig`, one function per measurable rule, each walking every
 * rendered element with `getComputedStyle`. A check returns findings; the
 * test filters them through the allow-list.
 *
 * Everything here runs in the browser, so `installChecks` may not refer to
 * anything outside its own body.
 */

/** The nine rules of web/DESIGN.md "Conformance". */
export type Rule = "palette" | "typography" | "contrast" | "hit-size" | "grid" | "materials" | "motion" | "keyboard" | "structure";

/** One violation: the rule, a CSS path to the element, and what was measured. */
export type Finding = { readonly rule: Rule; readonly where: string; readonly detail: string };

/** What a page check needs to know about the page. */
export type CheckOptions = {
  /** The smallest regular control, in px: 28 on desktop, 44 on a phone. */
  readonly controlMin: number;
  /** Desktop pages hold body text to 13 px. */
  readonly desktop: boolean;
};

/** Defines `globalThis.__hig` in the page. Serialised by Playwright: self-contained. */
export function installChecks(): void {
  type Hit = { el: Element; detail: string };
  const INTERACTIVE =
    'a[href], button, input:not([type="hidden"]), select, textarea, summary, [role="button"], [role="link"], [role="tab"], [role="checkbox"], [role="switch"], [role="option"], [role="menuitem"], [tabindex]:not([tabindex="-1"])';
  const CHROME = 'header, nav, aside, [role="banner"], [role="navigation"], [role="toolbar"], [role="dialog"], [role="menu"], [role="listbox"], [popover]';
  const ROWS = 'nav, [role="listbox"], [role="tablist"], [role="group"], [role="menu"], [role="tree"]';
  const css = (el: Element): CSSStyleDeclaration => getComputedStyle(el);
  const shown = (el: Element): boolean => el.checkVisibility({ visibilityProperty: true });
  const elements = (): Element[] => [...document.body.querySelectorAll("*")].filter(shown);
  const interactive = (): Element[] => [...document.body.querySelectorAll(INTERACTIVE)].filter(shown);
  const hasText = (el: Element): boolean => [...el.childNodes].some((node) => node.nodeType === Node.TEXT_NODE && node.textContent!.trim() !== "");
  const where = (el: Element): string => {
    const parts: string[] = [];
    for (let node: Element | null = el; node !== null && node !== document.body && parts.length < 4; node = node.parentElement) {
      const id = node.getAttribute("data-testid");
      parts.unshift(node.tagName.toLowerCase() + (id ? `[data-testid="${id}"]` : node.classList.length ? `.${[...node.classList].join(".")}` : ""));
    }
    return parts.join(" > ") || el.tagName.toLowerCase();
  };
  // A probe element resolves token values to the form getComputedStyle reports.
  const probe = document.createElement("span");
  probe.hidden = true;
  document.body.append(probe);
  const resolve = (property: "color" | "font-family", value: string): string => {
    probe.style.setProperty(property, value);
    return css(probe).getPropertyValue(property);
  };
  const tokenValue = (name: string): string => css(document.documentElement).getPropertyValue(name).trim();
  // Every --color-* token declared in the stylesheets, resolved in the active mode.
  const palette = (): Set<string> => {
    const names = new Set<string>();
    for (const sheet of document.styleSheets) {
      const walk = (rules: CSSRuleList): void => {
        for (const rule of rules) {
          if (rule instanceof CSSGroupingRule) walk(rule.cssRules);
          else for (const match of rule.cssText.matchAll(/(--color-[a-z0-9-]+)\s*:/g)) names.add(match[1]!);
        }
      };
      walk(sheet.cssRules);
    }
    return new Set(["rgba(0, 0, 0, 0)", ...[...names].map((name) => resolve("color", tokenValue(name)))]);
  };
  const rgba = (value: string): [number, number, number, number] => {
    const [r = 0, g = 0, b = 0, a = 1] = (value.match(/[\d.]+/g) ?? []).map(Number);
    return [r, g, b, value.startsWith("rgba") ? a : 1];
  };
  const over = (top: number[], bottom: number[]): [number, number, number, number] => {
    const a = top[3]! + bottom[3]! * (1 - top[3]!);
    const mix = (i: number): number => (a === 0 ? 0 : (top[i]! * top[3]! + bottom[i]! * bottom[3]! * (1 - top[3]!)) / a);
    return [mix(0), mix(1), mix(2), a];
  };
  // The element's background with every ancestor composited beneath it, over the white canvas.
  const background = (el: Element): number[] => {
    const layers: number[][] = [];
    for (let node: Element | null = el; node !== null; node = node.parentElement) {
      const layer = rgba(css(node).backgroundColor);
      layers.push(layer);
      if (layer[3] === 1) break;
    }
    return layers.reduceRight<number[]>((under, layer) => over(layer, under), [255, 255, 255, 1]);
  };
  const luminance = (c: number[]): number => {
    const [r, g, b] = c.slice(0, 3).map((v) => (v / 255 <= 0.04045 ? v / 255 / 12.92 : ((v / 255 + 0.055) / 1.055) ** 2.4));
    return 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;
  };
  const ratio = (a: number[], b: number[]): number => {
    const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
    return (hi! + 0.05) / (lo! + 0.05);
  };
  const name = (el: Element): string => {
    const labelled = el.getAttribute("aria-labelledby");
    const label = el.getAttribute("aria-label") ?? (labelled ? document.getElementById(labelled)?.textContent : null);
    if (label) return label;
    const labels = (el as HTMLInputElement).labels;
    if (labels && labels.length > 0) return [...labels].map((each) => each.textContent).join(" ");
    const visible = [...el.childNodes].filter((node) => !(node instanceof Element && node.closest('[aria-hidden="true"]')));
    return visible.map((node) => node.textContent ?? "").join("").trim() || (el.getAttribute("title") ?? "");
  };
  const report = (rule: string, hits: Hit[]): { rule: string; where: string; detail: string; el: Element }[] =>
    hits.map((hit) => ({ rule, where: where(hit.el), detail: hit.detail, el: hit.el }));

  const checks: Record<string, (options: { controlMin: number; desktop: boolean }) => Hit[]> = {
    palette: () => {
      const allowed = palette();
      const hits: Hit[] = [];
      for (const el of elements()) {
        const style = css(el);
        const props = ["color", "background-color"];
        for (const side of ["top", "right", "bottom", "left"]) {
          if (style.getPropertyValue(`border-${side}-style`) !== "none" && parseFloat(style.getPropertyValue(`border-${side}-width`)) > 0) props.push(`border-${side}-color`);
        }
        if (style.outlineStyle !== "none" && parseFloat(style.outlineWidth) > 0) props.push("outline-color");
        if (el instanceof SVGElement) props.push("fill", "stroke");
        for (const prop of props) {
          const value = style.getPropertyValue(prop);
          if (!allowed.has(value) && value !== "none" && !value.startsWith("url(")) hits.push({ el, detail: `${prop} ${value} is not a token` });
        }
      }
      return hits;
    },
    typography: (options) => {
      const families = new Set([tokenValue("--font-family-system"), tokenValue("--font-family-mono")].map((value) => resolve("font-family", value)));
      const hits: Hit[] = [];
      const body = parseFloat(css(document.body).fontSize);
      if (options.desktop && body !== 13) hits.push({ el: document.body, detail: `body text is ${body} px, not 13` });
      for (const el of elements().filter(hasText)) {
        const style = css(el);
        const size = parseFloat(style.fontSize);
        if (size < 10) hits.push({ el, detail: `${size} px text is under 10 px` });
        if (Number(style.fontWeight) < 300 && size < 15) hits.push({ el, detail: `weight ${style.fontWeight} at ${size} px` });
        if (!families.has(style.fontFamily)) hits.push({ el, detail: `font-family ${style.fontFamily} is not the system stack` });
      }
      return hits;
    },
    contrast: () => {
      const hits: Hit[] = [];
      for (const el of elements().filter(hasText)) {
        if (el.closest(':disabled, [aria-disabled="true"]')) continue;
        const style = css(el);
        const size = parseFloat(style.fontSize);
        const large = size >= 24 || (size >= 18.66 && Number(style.fontWeight) >= 700);
        const back = background(el);
        const value = ratio(over(rgba(style.color), back), back);
        if (value < (large ? 3 : 4.5)) hits.push({ el, detail: `contrast ${value.toFixed(2)}:1 is under ${large ? 3 : 4.5}:1` });
      }
      return hits;
    },
    "hit-size": (options) => {
      const hits: Hit[] = [];
      // Inline links in running text are exempt (WCAG 2.5.8), and so is a control scrolled out from under the pointer.
      const onTop = (el: Element): boolean => {
        const box = el.getBoundingClientRect();
        const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
        return hit !== null && (el.contains(hit) || hit.contains(el));
      };
      const controls = interactive().filter((el) => css(el).display !== "inline" && onTop(el));
      const boxes = controls.map((el) => el.getBoundingClientRect());
      // A control partly scrolled out of its scroller has no defined neighbour spacing.
      const clipped = controls.map((el, i) => {
        for (let node = el.parentElement; node !== null; node = node.parentElement) {
          if (!/(auto|scroll|hidden)/.test(css(node).overflow)) continue;
          const view = node.getBoundingClientRect();
          const box = boxes[i]!;
          if (box.top < view.top - 0.5 || box.bottom > view.bottom + 0.5 || box.left < view.left - 0.5 || box.right > view.right + 0.5) return true;
        }
        return false;
      });
      controls.forEach((el, i) => {
        const box = boxes[i]!;
        const min = el.closest('[data-control-size="small"]') ? 20 : options.controlMin;
        if (box.width < min - 0.5 || box.height < min - 0.5) hits.push({ el, detail: `${box.width.toFixed(1)}x${box.height.toFixed(1)} is under ${min}x${min}` });
        controls.forEach((other, j) => {
          if (j <= i || el.contains(other) || other.contains(el) || clipped[i] || clipped[j]) return;
          const b = boxes[j]!;
          const gap = Math.max(b.left - box.right, box.left - b.right, b.top - box.bottom, box.top - b.bottom);
          // Rows of a list and segments of a group may touch; other neighbours keep one grid step apart.
          const rows = gap >= -0.5 && el.closest(ROWS) !== null && el.closest(ROWS) === other.closest(ROWS);
          if (gap < 4 - 0.5 && !rows) {
            hits.push({ el, detail: `${gap.toFixed(1)} px from ${where(other)}, under 4 px` });
          }
        });
      });
      return hits;
    },
    grid: () => {
      const hits: Hit[] = [];
      const props = ["margin-top", "margin-right", "margin-bottom", "margin-left", "padding-top", "padding-right", "padding-bottom", "padding-left", "row-gap", "column-gap"];
      for (const el of elements()) {
        const map = el.computedStyleMap();
        for (const prop of props) {
          const value = map.get(prop);
          if (!(value instanceof CSSUnitValue) || value.unit !== "px") continue;
          const px = Math.abs(value.value);
          if (px !== 1 && Math.abs(px - 4 * Math.round(px / 4)) > 0.01) hits.push({ el, detail: `${prop} ${value.value} px is off the 4 px grid` });
        }
      }
      return hits;
    },
    materials: () =>
      elements()
        .filter((el) => css(el).backdropFilter !== "none" || /blur/.test(css(el).filter))
        .filter((el) => el.closest("main") !== null || el.closest(CHROME) === null)
        .map((el) => ({ el, detail: `blur (${css(el).backdropFilter}) outside a toolbar, sidebar or popover` })),
    motion: () => {
      const hits: Hit[] = [];
      for (const el of elements()) {
        const style = css(el);
        const durations = style.transitionDuration.split(",").map(parseFloat);
        style.transitionProperty.split(",").forEach((prop, i) => {
          if (prop.trim() !== "opacity" && (durations[i % durations.length] ?? 0) > 0) hits.push({ el, detail: `transition on ${prop.trim()} under reduced motion` });
        });
        for (const animation of el.getAnimations()) {
          const keys = (animation.effect as KeyframeEffect | null)?.getKeyframes() ?? [];
          const moved = keys.flatMap((frame) => Object.keys(frame)).filter((key) => !["offset", "easing", "composite", "computedOffset", "opacity"].includes(key));
          if (moved.length > 0) hits.push({ el, detail: `animation of ${[...new Set(moved)].join(", ")} under reduced motion` });
        }
      }
      return hits;
    },
    keyboard: () =>
      interactive()
        .filter((el) => !/[\p{L}\p{N}]/u.test(name(el)))
        .map((el) => ({ el, detail: `control has no accessible name (${JSON.stringify(name(el))})` })),
    structure: () => {
      const hits: Hit[] = [];
      for (const head of document.querySelectorAll('nav [aria-expanded], [role="navigation"] [aria-expanded]')) {
        let depth = 0;
        for (let node = head.parentElement; node !== null && node.tagName !== "NAV"; node = node.parentElement) {
          if ([...node.children].some((child) => child.hasAttribute("aria-expanded"))) depth += 1;
        }
        if (depth > 2) hits.push({ el: head, detail: `sidebar group ${depth} levels deep, over 2` });
      }
      for (const el of interactive()) {
        if (/\b(dark|light) (mode|theme)\b|\btheme\b|\bappearance\b/i.test(name(el))) hits.push({ el, detail: `app-specific appearance switch "${name(el)}"` });
      }
      return hits;
    },
  };
  Object.assign(globalThis, {
    __hig: {
      run: (rule: string, options: { controlMin: number; desktop: boolean }, allowed: { rule: string; selector: string }[]) => {
        const all = report(rule, checks[rule]!(options));
        const used = new Set<number>();
        const open = all.filter((hit) => {
          const index = allowed.findIndex((entry) => entry.rule === rule && hit.el.matches(entry.selector));
          if (index >= 0) used.add(index);
          return index < 0;
        });
        return { findings: open.map(({ el: _el, ...rest }) => rest), used: [...used] };
      },
      // Tab order and focus rings: every reachable control in `seen`, each without a ring in `ringless`.
      // A list's rows are reached with the arrow keys, not Tab (WAI-ARIA APG, "Keyboard navigation inside components").
      interactive: () => interactive().filter((el) => !el.matches(':disabled, [role="option"], [role="treeitem"], [role="menuitem"]') && !el.closest("[inert]")).map(where),
      focusRing: () => {
        const el = document.activeElement;
        if (el === null || el === document.body) return null;
        const style = css(el);
        const ring = (style.outlineStyle !== "none" && parseFloat(style.outlineWidth) > 0) || style.boxShadow !== "none";
        return { where: where(el), ring };
      },
      // The listbox-like widgets on screen and a signature of their selection.
      lists: () => [...document.querySelectorAll('[role="listbox"], [role="tree"], [role="menu"], [role="grid"], [role="tablist"]')].filter(shown).map(where),
      selection: () => {
        const focused = document.activeElement;
        const selected = [...document.querySelectorAll('[aria-selected="true"]')].map((el) => `${where(el)}#${[...(el.parentElement?.children ?? [])].indexOf(el)}`).join("|");
        return `${focused?.getAttribute("aria-activedescendant") ?? ""}/${selected}/${focused ? where(focused) : ""}`;
      },
      focusList: (path: string) => {
        const list = [...document.querySelectorAll('[role="listbox"], [role="tree"], [role="menu"], [role="grid"], [role="tablist"]')].find((el) => where(el) === path);
        if (list === undefined) return false;
        const controller = list.id ? document.querySelector(`[aria-controls="${list.id}"]`) : null;
        const target = (controller ?? (list.matches("[tabindex]") ? list : list.querySelector('[aria-selected="true"], [tabindex]'))) as HTMLElement | null;
        target?.focus();
        return target !== null;
      },
      where: () => (document.activeElement ? where(document.activeElement) : ""),
      toolbarActions: () =>
        [...document.querySelectorAll('header button, [role="toolbar"] button')]
          .filter((el) => shown(el) && !el.matches(`:disabled, [aria-disabled="true"], [aria-haspopup], [data-testid="palette-trigger"]`))
          .map(name),
      commands: () => [...document.querySelectorAll('[role="dialog"] [role="option"]')].map((el) => el.textContent ?? ""),
    },
  });
}
