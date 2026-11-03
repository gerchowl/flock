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
import { For, createSignal, type JSX } from "solid-js";

import { Center, Cluster, Grid, Sidebar, Stack, Switcher } from "../layout/primitives";
import "./gallery.css";

const OPTIONS = ["Review the pull request", "Run the test suite", "Summarise the log"] as const;

/** A list with one selected option that the arrow keys move (HIG: lists and tables). */
function SelectionList(): JSX.Element {
  const [selected, setSelected] = createSignal(0);
  const onKey = (event: KeyboardEvent): void => {
    const step = event.key === "ArrowDown" ? 1 : event.key === "ArrowUp" ? -1 : 0;
    if (step === 0) return;
    event.preventDefault();
    setSelected((selected() + step + OPTIONS.length) % OPTIONS.length);
  };
  return (
    <Stack as="div" space="1" class="g-list" role="listbox" aria-label="Tasks" tabindex="0" onKeyDown={onKey}>
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
    <Stack as="section" space="1">
      <button type="button" class="g-button" aria-expanded={open()} onClick={() => setOpen(!open())}>
        {props.title}
      </button>
      {open() ? props.children : null}
    </Stack>
  );
}

/** The gallery page. */
export function Gallery(): JSX.Element {
  const [pressed, setPressed] = createSignal(false);
  return (
    <div>
      <Sidebar side="left" sideWidth="240px" contentMin="50%" space="4">
        <nav class="g-sidebar" aria-label="Projects">
          <Group title="flock">
            <Group title="web client">
              <button type="button" class="g-button">
                Design rules
              </button>
            </Group>
          </Group>
        </nav>
        <Center as="main" max="960px" gutters="4">
          <Stack space="6">
            <h1>Gallery</h1>
            <Stack as="section" space="2" aria-label="Text">
              <h2>Text</h2>
              <p>
                Body text, the label colour, 13 px, with{" "}
                <a class="g-link" href="#text">
                  a link
                </a>
                .
              </p>
              <p class="g-secondary">Secondary text, 11 px.</p>
              <Cluster space="3">
                <span class="g-green">Working</span>
                <span class="g-orange">Needs you</span>
                <span class="g-red">Failed</span>
              </Cluster>
            </Stack>
            <Stack as="section" space="2" aria-label="Controls">
              <h2>Controls</h2>
              <Cluster space="2">
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
            <Stack as="section" space="2" aria-label="Layout">
              <h2>Layout</h2>
              <Switcher threshold="480px" space="2">
                <div class="g-panel">Switcher</div>
                <div class="g-panel">one row, or a column</div>
              </Switcher>
              <Grid min="160px" space="2">
                <div class="g-panel">Grid</div>
                <div class="g-panel">of equal</div>
                <div class="g-panel">columns</div>
              </Grid>
            </Stack>
          </Stack>
        </Center>
      </Sidebar>
    </div>
  );
}
