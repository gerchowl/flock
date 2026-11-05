import { For, createSignal, onCleanup, type JSX } from "solid-js";
import { bindKeys } from "../../workbench/keys";
import { PRESETS, type SplitPreset, type SplitState } from "../splits";
import "./split.css";

/** Static token presets; pointer movement previews a target, release commits it. */
export function Split(props: {
  readonly region: "sidebar" | "inspector";
  readonly value: SplitState;
  readonly change: (value: SplitState) => void;
  readonly children: JSX.Element;
  readonly rail?: JSX.Element;
}): JSX.Element {
  const [target, setTarget] = createSignal<SplitPreset>();
  const measures = new Map<SplitPreset, HTMLSpanElement>();
  let drag: { id: number; x: number; width: number } | undefined;
  const label = (): string => props.region === "sidebar" ? "Sidebar" : "Inspector";
  const toggle = (): void => props.change({ ...props.value, collapsed: !props.value.collapsed });
  const step = (delta: number): void => {
    const preset = PRESETS[Math.max(0, Math.min(2, PRESETS.indexOf(props.value.preset) + delta))]!;
    props.change({ ...props.value, preset });
  };
  const move = (event: PointerEvent): void => {
    if (drag === undefined || drag.id !== event.pointerId) return;
    const width = drag.width + (event.clientX - drag.x) * (props.region === "sidebar" ? 1 : -1);
    setTarget(PRESETS.reduce((a, b) => Math.abs(measures.get(a)!.getBoundingClientRect().width - width) <= Math.abs(measures.get(b)!.getBoundingClientRect().width - width) ? a : b));
  };
  const cancel = (): void => { drag = undefined; setTarget(undefined); };
  return (
    <div class="l-split" data-region={props.region} data-split={props.value.preset} data-collapsed={props.value.collapsed} data-target={target()}>
      <div class="l-split__content" id={`split-${props.region}`} inert={props.value.collapsed}><div class="l-split__full">{props.children}</div><div class="l-split__rail">{props.rail}</div></div>
      <div class="l-split__separator" role="separator" tabindex="0" aria-label={`${label()} width`} aria-orientation="vertical"
        aria-controls={`split-${props.region}`} aria-valuemin={0} aria-valuemax={3}
        aria-valuenow={props.value.collapsed ? 0 : PRESETS.indexOf(props.value.preset) + 1}
        aria-valuetext={props.value.collapsed ? "collapsed" : props.value.preset}
        ref={element => onCleanup(bindKeys(element, { "split.previous": () => step(-1), "split.next": () => step(1), "split.toggle": toggle }))}
        onDblClick={toggle}
        onPointerDown={event => {
          if (event.button !== 0 || props.value.collapsed) return;
          event.currentTarget.setPointerCapture(event.pointerId);
          event.currentTarget.focus();
          drag = { id: event.pointerId, x: event.clientX, width: measures.get(props.value.preset)!.getBoundingClientRect().width };
          setTarget(props.value.preset);
        }}
        onPointerMove={move}
        onPointerUp={event => {
          if (drag?.id !== event.pointerId) return;
          move(event);
          props.change({ ...props.value, preset: target() ?? props.value.preset });
          cancel();
        }}
        onPointerCancel={cancel} onLostPointerCapture={cancel}
      ><span class="l-split__target" role="status">{target()}</span></div>
      <For each={PRESETS}>{preset => <span class="l-split__measure" data-split={preset} aria-hidden="true" ref={element => measures.set(preset, element)} />}</For>
    </div>
  );
}

/** The desktop chrome and three regions share one grid owned by layout. */
export function SplitShell(props: { readonly children: JSX.Element; readonly active: boolean }): JSX.Element {
  return <div class="workbench l-split-shell" data-testid="workbench" data-window={props.active ? "active" : "inactive"}>{props.children}</div>;
}
