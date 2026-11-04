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

/**
 * One violation: the rule, a CSS path to the element, what was measured, and
 * the narrowest selector naming the element (what an allow-list entry uses).
 */
export type Finding = { readonly rule: Rule; readonly where: string; readonly detail: string; readonly selector: string };

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
  // Containers whose rows or segments may touch: lists, tab bars, segmented groups and menus.
  const ROWS = 'ul, ol, [role="list"], [role="listbox"], [role="tablist"], [role="group"], [role="menu"], [role="tree"]';
  const LISTS = '[role="listbox"], [role="tree"], [role="menu"], [role="grid"], [role="tablist"]';
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
  // The element's own selector: its test id, else its classes, else its tag under its parent's.
  const selectorOf = (el: Element): string => {
    const id = el.getAttribute("data-testid");
    if (id) return `[data-testid="${id}"]`;
    if (el.classList.length > 0) return `.${[...el.classList].join(".")}`;
    const tag = el.tagName.toLowerCase();
    return el.parentElement === null || el.parentElement === document.body || tag === "body" ? tag : `${selectorOf(el.parentElement)} > ${tag}`;
  };
  // Element identities for the keyboard probes: a path can name two elements, an id cannot.
  const ids = new WeakMap<Element, number>();
  let nextId = 0;
  const idOf = (el: Element): number => {
    if (!ids.has(el)) ids.set(el, nextId++);
    return ids.get(el)!;
  };
  // An element's outline (when one is drawn) and box-shadow, at rest and when focused.
  type Ring = { readonly outline: string | null; readonly shadow: string };
  const resting = new Map<number, Ring>();
  const ringOf = (el: Element): Ring => {
    const style = css(el);
    const drawn = style.outlineStyle !== "none" && parseFloat(style.outlineWidth) > 0;
    return { outline: drawn ? `${style.outlineStyle} ${style.outlineWidth} ${style.outlineColor} ${style.outlineOffset}` : null, shadow: style.boxShadow };
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
  /** The text a control shows, without its hidden decoration (an icon's glyph). */
  const ownText = (el: Element): string =>
    [...el.childNodes]
      .filter((node) => !(node instanceof Element && node.closest('[aria-hidden="true"]')))
      .map((node) => node.textContent ?? "")
      .join("")
      .trim();
  const name = (el: Element): string => {
    const labelled = el.getAttribute("aria-labelledby");
    const label = el.getAttribute("aria-label") ?? (labelled ? document.getElementById(labelled)?.textContent : null);
    if (label) return label;
    const labels = (el as HTMLInputElement).labels;
    if (labels && labels.length > 0) return [...labels].map((each) => each.textContent).join(" ");
    return ownText(el) || (el.getAttribute("title") ?? "");
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
      // Every control is measured, on screen or not; only links in running text are exempt (WCAG 2.5.8).
      const controls = interactive().filter((el) => css(el).display !== "inline");
      // A control covered by another (under a sticky bar) or partly scrolled out of its scroller has no
      // defined neighbour spacing. Occlusion is read with each control scrolled into view, then every
      // scroll position is restored so all boxes are measured in one state.
      const scrollers = [document.scrollingElement!, ...elements().filter((el) => el.scrollHeight > el.clientHeight || el.scrollWidth > el.clientWidth)];
      const saved = scrollers.map((el) => [el.scrollTop, el.scrollLeft] as const);
      const covered = controls.map((el) => {
        el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
        const box = el.getBoundingClientRect();
        const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
        return hit === null || !(el.contains(hit) || hit.contains(el));
      });
      scrollers.forEach((el, i) => el.scrollTo({ top: saved[i]![0], left: saved[i]![1], behavior: "instant" }));
      const boxes = controls.map((el) => el.getBoundingClientRect());
      const clipped = controls.map((el, i) => {
        if (covered[i]) return true;
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
    keyboard: () => {
      const named = /[\p{L}\p{N}]/u;
      const hits: Hit[] = [];
      for (const el of interactive()) {
        if (!named.test(name(el))) hits.push({ el, detail: `control has no accessible name (${JSON.stringify(name(el))})` });
        // An icon-only control (#2903) shows no words, so an aria-label names it: a tooltip (`title`) is not a name.
        else if (!named.test(ownText(el)) && !el.hasAttribute("aria-labelledby") && !named.test(el.getAttribute("aria-label") ?? "") && !(el as HTMLInputElement).labels?.length)
          hits.push({ el, detail: "icon-only control without an aria-label" });
      }
      return hits;
    },
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
        return { findings: open.map(({ el, ...rest }) => ({ ...rest, selector: selectorOf(el) })), used: [...used] };
      },
      // Tab targets: every enabled control except a list's rows, which the arrow keys reach
      // (WAI-ARIA APG, "Keyboard navigation inside components"). Their resting ring style is
      // recorded with nothing focused, so a focus ring must differ from it.
      tabTargets: () => {
        (document.activeElement as HTMLElement | null)?.blur();
        return interactive()
          .filter((el) => !el.matches(':disabled, [role="option"], [role="treeitem"], [role="menuitem"]') && !el.closest("[inert]"))
          .map((el) => {
            resting.set(idOf(el), ringOf(el));
            return { id: idOf(el), where: where(el) };
          });
      },
      // A focus ring is an outline drawn only on focus, or a box-shadow that differs from the resting one.
      focused: () => {
        const el = document.activeElement;
        if (el === null || el === document.body) return null;
        const now = ringOf(el);
        const before = resting.get(idOf(el)) ?? { outline: null, shadow: "none" };
        const ring = (now.outline !== null && now.outline !== before.outline) || (now.shadow !== "none" && now.shadow !== before.shadow);
        return { id: idOf(el), where: where(el), ring };
      },
      lists: () => [...document.querySelectorAll(LISTS)].filter(shown).map((el) => ({ id: idOf(el), where: where(el) })),
      selection: () => {
        const focused = document.activeElement;
        const selected = [...document.querySelectorAll('[aria-selected="true"]')].map((el) => idOf(el)).join("|");
        return `${focused?.getAttribute("aria-activedescendant") ?? ""}/${selected}/${focused ? idOf(focused) : ""}`;
      },
      // Focuses the list's controller (a combobox), the list itself, or its selected or focusable row.
      focusList: (id: number) => {
        const list = [...document.querySelectorAll(LISTS)].find((el) => ids.get(el) === id);
        if (list === undefined) return false;
        const controller = list.id ? document.querySelector(`[aria-controls="${list.id}"]`) : null;
        const target = (controller ?? (list.matches("[tabindex]") ? list : list.querySelector('[aria-selected="true"][tabindex], [tabindex]'))) as HTMLElement | null;
        target?.focus();
        return target !== null && document.activeElement === target;
      },
      focusFirst: () => {
        const first = interactive().find((el) => !el.matches(':disabled, [role="option"]') && el.closest('[role="dialog"]') === null) as HTMLElement | undefined;
        first?.focus();
        return first !== undefined && document.activeElement === first ? idOf(first) : null;
      },
      focusedId: () => (document.activeElement && document.activeElement !== document.body ? idOf(document.activeElement) : null),
      // Toolbars: every `[role="toolbar"]` and `header` on screen, with the names of all their
      // buttons, disabled ones included (an unavailable action still needs its command).
      toolbars: () =>
        [...document.querySelectorAll('[role="toolbar"], header')]
          .filter((el) => shown(el) && el.closest('[role="dialog"]') === null && !(el.matches("header") && el.closest('[role="toolbar"]') !== null))
          .map((bar) => ({
            where: where(bar),
            actions: [...bar.querySelectorAll("button")].filter((el) => shown(el) && !el.matches('[data-testid="palette-trigger"]')).map(name),
          })),
      // The same toolbars' rendered buttons, counted apart from the walk above: parity
      // compares the two, so a walk that misses a toolbar's actions fails instead of passing empty.
      toolbarButtons: () =>
        [...document.querySelectorAll('[role="toolbar"], header')]
          .filter((el) => shown(el) && el.closest('[role="dialog"]') === null && !(el.matches("header") && el.closest('[role="toolbar"]') !== null))
          .map((bar) => ({ where: where(bar), buttons: [...bar.querySelectorAll("button")].filter((el) => shown(el) && el.getAttribute("data-testid") !== "palette-trigger").length })),
      // What a walk covers, and the media the page itself sees, so a hidden page or a dropped mode shows.
      coverage: () => ({
        elements: elements().length,
        controls: interactive().length,
        scheme: matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light",
        contrast: matchMedia("(prefers-contrast: more)").matches ? "more" : "no-preference",
        reduced: matchMedia("(prefers-reduced-motion: reduce)").matches,
      }),
      commands: () => [...document.querySelectorAll('[role="dialog"] [role="option"]')].map((el) => el.textContent ?? ""),
      dialogOpen: () => [...document.querySelectorAll('[role="dialog"]')].some(shown) && document.activeElement?.closest('[role="dialog"]') !== null,
    },
  });
}
