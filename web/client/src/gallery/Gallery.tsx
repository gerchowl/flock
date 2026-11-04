/**
 * The gallery (#2896): every layout primitive and every control state on
 * one page, styled from tokens only, so the design-conformance suite can
 * check the rules of web/DESIGN.md on each of them in every colour mode.
 * The workbench screens are checked separately, from the fixture build.
 * It has no toolbar, so it needs no command palette: rule 9's parity check
 * applies to the workbench's toolbars.
 *
 * @module
 */
import { For, createSignal, onCleanup, type JSX } from "solid-js";

import { ICON_NAMES, Icon, IconButton } from "../components/Icon";
import { Center, Cluster, Grid, Sidebar, Stack, Sticky, Switcher } from "../layout/primitives";
import { SPACE } from "../tokens";
import { HEALTH_LABELS, HostStatus, type Health } from "../workbench/HostStatus";
import { bindKeys } from "../workbench/keys";
import { ErrorState, RefusalDetails, explain } from "../workbench/refusals";
import { ShortcutList } from "../workbench/ShortcutSheet";
import "./gallery.css";

/** A refusal the server sends, worded in the shared map's plain words, its code behind Details. */
const BUSY = { code: "session_busy", message: "command refused: session_busy" } as const;

const OPTIONS = ["Review the pull request", "Run the test suite", "Summarise the log"] as const;

/** A list with one selected option that the arrow keys move (HIG: lists and tables), through the keyboard layer. */
function SelectionList(): JSX.Element {
  const [selected, setSelected] = createSignal(0);
  const step = (by: 1 | -1): void => {
    setSelected((selected() + by + OPTIONS.length) % OPTIONS.length);
  };
  const bind = (list: HTMLElement): void => {
    onCleanup(bindKeys(list, { "listbox.next": () => step(1), "listbox.previous": () => step(-1) }));
  };
  return (
    <Stack ref={bind} as="div" space={SPACE[1]} class="g-list" role="listbox" aria-label="Tasks" tabindex="0">
      <For each={OPTIONS}>
        {(label, index) => (
          <div class="g-option" role="option" aria-selected={index() === selected()} onClick={() => setSelected(index())}>
            {label}
          </div>
        )}
      </For>
    </Stack>
  );
}

/** A collapsible group: the sidebar's tree, two levels deep at most. */
function Group(props: { readonly title: string; readonly children: JSX.Element }): JSX.Element {
  const [open, setOpen] = createSignal(true);
  return (
    <Stack as="section" space={SPACE[1]}>
      <button type="button" class="g-button" aria-expanded={open()} onClick={() => setOpen(!open())}>
        {props.title}
      </button>
      {open() ? props.children : null}
    </Stack>
  );
}

/** The toolbar's connection control in one health, its popover open or closed. */
function Host(props: { readonly health: Health; readonly open?: true }): JSX.Element {
  const [open, setOpen] = createSignal(props.open === true);
  const view = { name: "node-a", health: () => props.health, detail: () => HEALTH_LABELS[props.health], diagnosis: () => null, connected: () => props.health !== "off", connect: () => undefined, disconnect: () => undefined };
  return <HostStatus view={view} name={`node-a (${HEALTH_LABELS[props.health]})`} open={open} setOpen={setOpen} />;
}

/** The gallery page. */
export function Gallery(): JSX.Element {
  const [pressed, setPressed] = createSignal(false);
  return (
    <div>
      <Sidebar side="left" sideWidth="240px" contentMin="50%" space={SPACE[4]}>
        <Sticky as="nav" class="g-sidebar" aria-label="Projects">
          <Group title="flock">
            <Group title="web client">
              <button type="button" class="g-button">
                Design rules
              </button>
            </Group>
          </Group>
        </Sticky>
        <Center as="main" max="960px" gutters={SPACE[4]}>
          <Stack space={SPACE[6]}>
            <h1>Gallery</h1>
            <Stack as="section" space={SPACE[2]} aria-label="Text">
              <h2>Text</h2>
              <p>
                Body text, the label colour, 13 px, with{" "}
                <a class="g-link" href="#text">
                  a link
                </a>
                .
              </p>
              <p class="g-secondary">Secondary text, 11 px.</p>
              <Cluster space={SPACE[3]}>
                <span class="g-green">Working</span>
                <span class="g-orange">Needs you</span>
                <span class="g-red">Failed</span>
              </Cluster>
            </Stack>
            <Stack as="section" space={SPACE[2]} aria-label="Controls">
              <h2>Controls</h2>
              <Cluster space={SPACE[2]}>
                <button type="button" class="g-button">
                  Default
                </button>
                <button type="button" class="g-button" aria-pressed={pressed()} onClick={() => setPressed(!pressed())}>
                  Pin
                </button>
                <button type="button" class="g-button" aria-label="Add">
                  +
                </button>
                <button type="button" class="g-button g-button--primary">
                  Primary
                </button>
                <button type="button" class="g-button g-button--destructive">
                  Delete
                </button>
                <button type="button" class="g-button" disabled>
                  Disabled
                </button>
              </Cluster>
              <SelectionList />
            </Stack>
            <Stack as="section" space={SPACE[2]} aria-label="Icons">
              <h2>Icons</h2>
              <Cluster space={SPACE[2]}>
                <For each={ICON_NAMES}>{(name) => <IconButton icon={name} label={`Icon ${name}`} />}</For>
                <IconButton icon="send" label="Send, disabled" disabled />
                <button type="button" class="g-button">
                  <Icon name="search" size={14} /> With a label
                </button>
              </Cluster>
            </Stack>
            <Stack as="section" space={SPACE[2]} aria-label="Layout">
              <h2>Layout</h2>
              <Switcher threshold="480px" space={SPACE[2]}>
                <div class="g-panel">Switcher</div>
                <div class="g-panel">one row, or a column</div>
              </Switcher>
              <Grid min="160px" space={SPACE[2]}>
                <div class="g-panel">Grid</div>
                <div class="g-panel">of equal</div>
                <div class="g-panel">columns</div>
              </Grid>
            </Stack>
            <Stack as="section" space={SPACE[2]} aria-label="Connection">
              <h2>Connection</h2>
              <Cluster space={SPACE[2]}>
                <Host health="pending" />
                <Host health="problem" />
                <Host health="off" />
                <Host health="live" open />
              </Cluster>
            </Stack>
            <Stack as="section" space={SPACE[2]} aria-label="Shortcuts">
              <h2>Shortcuts</h2>
              <ShortcutList />
            </Stack>
            <Stack as="section" space={SPACE[2]} aria-label="Refusals">
              <h2>Refusals</h2>
              <ErrorState title="Couldn't load this agent's history." line={explain(BUSY).line} code={BUSY.code} message={BUSY.message} retry={() => undefined} />
              <span>{explain(BUSY).line}</span>
              <RefusalDetails code={BUSY.code} message={BUSY.message} open />
            </Stack>
          </Stack>
        </Center>
      </Sidebar>
    </div>
  );
}
