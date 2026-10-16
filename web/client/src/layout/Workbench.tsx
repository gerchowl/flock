import { For, Show, createSignal, onCleanup, type JSX } from "solid-js";
import { App } from "../App";
import { ReplayBadge } from "../components/ReplayBadge";
import type { SessionStream } from "../transport";
import type { ConfigResult } from "../config";
import { createFixtureTransport } from "../transport/fixture";
import { Transcript } from "../views/Transcript";
import { createManager, kinds, tileIds, type Callbacks, type Kind, type Layout, type Placement, type Tile, type Tree } from "./model";
import { createLayoutStore } from "./persistence";
import { isPhone, type Mode } from "./phone";
import "./workbench.css";
/** Stable serverKey follows persistence.ts's identity contract. Mode defaults to auto. */
export type WorkbenchProps = { serverKey: string; mode?: Mode; initial?: Layout; callbacks?: Callbacks; body?: (tile: Tile) => JSX.Element };
/** Keyboard: Alt+ArrowLeft/Right cycles tiles; all operations also have native controls. */
export function Workbench(props: WorkbenchProps): JSX.Element {
  const store = createLayoutStore({ serverKey: props.serverKey });
  const loaded = store.load();
  const [state, setState] = createSignal(props.initial ?? loaded.layout);
  const [notice, setNotice] = createSignal(loaded.notice);
  const [width, setWidth] = createSignal(window.innerWidth);
  const [sheet, setSheet] = createSignal(false);
  const [subject, setSubject] = createSignal("agent/fixture");
  const [kind, setKind] = createSignal<Kind>("transcript");
  const [placement, setPlacement] = createSignal<Placement | "default">("default");
  const phone = () => isPhone(props.mode ?? "auto", width());
  const manager = () => createManager(state(), props.callbacks, phone());
  const publish = () => { setState({ ...state() }); const error = store.save(state()); if (error) setNotice(error); };
  const run = (action: (m: ReturnType<typeof createManager>) => void) => { action(manager()); publish(); };
  const resize = () => setWidth(window.innerWidth);
  const historyKey = `flock-layout:${props.serverKey}`;
  const depthKey = `${historyKey}:depth`;
  const [depth, setDepth] = createSignal<number>(window.history.state?.[depthKey] ?? 0);
  const push = (action: (m: ReturnType<typeof createManager>) => void) => {
    run(action);
    if (phone()) { setDepth(depth() + 1); window.history.pushState({ [historyKey]: state().focused, [depthKey]: depth() }, ""); }
  };
  const pop = (event: PopStateEvent) => run(m => {
    setDepth(event.state?.[depthKey] ?? 0);
    const id: unknown = event.state?.[historyKey];
    if (typeof id === "string" && state().tiles[id]?.mode === "Expanded") { m.focus(id); state().history = state().history.slice(0, state().history.lastIndexOf(id)); }
    else m.back();
  });
  window.history.replaceState({ ...window.history.state, [historyKey]: state().focused }, "");
  window.addEventListener("resize", resize); window.addEventListener("popstate", pop);
  onCleanup(() => { window.removeEventListener("resize", resize); window.removeEventListener("popstate", pop); });
  const focusControl = (id: string) => { run(m => m.focus(id)); document.getElementById(`tile-${id}`)?.focus(); };
  const keys = (event: KeyboardEvent) => {
    if (!event.altKey || !["ArrowLeft", "ArrowRight"].includes(event.key)) return;
    event.preventDefault();
    const ids = tileIds(state().tree), index = ids.indexOf(state().focused ?? "");
    const id = ids[(index + (event.key === "ArrowRight" ? 1 : ids.length - 1)) % ids.length];
    if (id) focusControl(id);
  };
  const TileView = (p: { id: string }) => {
    const tile = () => state().tiles[p.id]!;
    return <section id={`tile-${p.id}`} class="wb-tile" tabindex="-1" aria-label={`${tile().kind}: ${tile().subject}`} onFocusIn={() => { if (state().focused !== p.id) run(m => m.focus(p.id)); }}>
      <header><strong>{tile().kind} · {tile().subject}</strong><button onClick={() => run(m => m.collapse(p.id))}>Collapse</button><button onClick={() => run(m => m.close(p.id))}>Close</button><button onClick={() => push(m => m.expandIntoSecond(subject(), p.id, kind()))}>Expand into second</button></header>
      {props.body ? props.body(tile()) : <p>{tile().kind}: not implemented yet</p>}
    </section>;
  };
  const Node = (p: { tree: Tree }): JSX.Element => <Show when={p.tree.type === "stack" ? p.tree : null} fallback={(() => {
    if (p.tree.type !== "split") return null;
    return <div class={`wb-split wb-${p.tree.direction}`}><For each={p.tree.children}>{(child, i) => <div style={{ flex: `${p.tree.type === "split" ? p.tree.weights[i()] : 1} 1 0%` }}><Node tree={child} /></div>}</For></div>;
  })()}>{leaf => <div class="wb-stack"><nav aria-label="Stack tabs"><For each={leaf().tiles}>{id => <button aria-pressed={leaf().active === id} onClick={() => focusControl(id)}>{state().tiles[id]?.subject}</button>}</For></nav><TileView id={leaf().active} /></div>}</Show>;
  const chips = () => <For each={state().rail}>{id => <div class="wb-chip"><button onClick={() => { run(m => m.restore(id)); setSheet(false); }}>{state().tiles[id]?.subject} {state().tiles[id]?.badge}<time>{new Date(state().tiles[id]!.changed).toLocaleTimeString()}</time></button><button aria-label={`Close ${state().tiles[id]?.subject}`} onClick={() => run(m => m.close(id))}>Close</button></div>}</For>;
  return <main class="workbench" data-mode={phone() ? "phone" : "desktop"} onKeyDown={keys}>
    <Show when={notice()}><p role="alert">{notice()}</p></Show>
    <form class="wb-tools" onSubmit={event => { event.preventDefault(); if (subject().trim()) { const place = placement(); const action = (m: ReturnType<typeof createManager>) => m.open(subject().trim(), place === "default" ? undefined : place, kind()); if (phone() && (place === "default" || place === "push")) push(action); else run(action); } }}>
      <input aria-label="Subject" value={subject()} onInput={e => setSubject(e.currentTarget.value)} required />
      <select aria-label="Tile kind" value={kind()} onChange={e => setKind(e.currentTarget.value as Kind)}><For each={kinds}>{k => <option>{k}</option>}</For></select>
      <select aria-label="Placement" onChange={e => setPlacement(e.currentTarget.value as Placement | "default")}><For each={["default", "replace", "tab", "split_right", "split_down", "push"]}>{p => <option>{p}</option>}</For></select><button type="submit">Open</button>
    </form>
    <Show when={phone()} fallback={<div class="wb-desktop"><div class="wb-canvas"><Show when={state().tree}>{tree => <Node tree={tree()} />}</Show></div><aside aria-label="Rail">{chips()}</aside></div>}>
      <nav aria-label="Tile switcher"><button disabled={!state().history.length} onClick={() => { if (depth() > 0) window.history.back(); else run(m => m.back()); }}>Back</button><For each={tileIds(state().tree)}>{id => <button aria-pressed={state().focused === id} onClick={() => focusControl(id)}>{state().tiles[id]?.subject}</button>}</For><button aria-expanded={sheet()} onClick={() => setSheet(!sheet())}>Rail ({state().rail.length})</button></nav>
      <Show when={state().focused}>{id => <TileView id={id()} />}</Show>
      <Show when={sheet()}><aside class="wb-sheet" role="dialog" aria-label="Rail" onKeyDown={e => { if (e.key === "Escape") setSheet(false); }}><button autofocus onClick={() => setSheet(false)}>Close rail</button>{chips()}</aside></Show>
    </Show>
    <Show when={!tileIds(state().tree).length}><p>No expanded tiles. Open a subject or restore a rail chip.</p></Show>
  </main>;
}
/** Shell bridge: fixture identity is literal; live mode is left to G3 #1595. */
export function ClientWorkbench(props: { config: ConfigResult; serverKey: string; mode: Mode }): JSX.Element {
  const config = props.config;
  if (!config.ok || config.config.transport !== "fixture") return <App config={config} />;
  const current = config.config, transport = createFixtureTransport(current.fixtureJsonl);
  return <><ReplayBadge source={current.source} /><Workbench serverKey={props.serverKey} mode={props.mode} body={tile => tile.kind === "transcript" ? <Transcript transport={transport} stream={tile.subject as SessionStream} /> : <p>{tile.kind}: not implemented yet</p>} /></>;
}
