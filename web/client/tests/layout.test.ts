import { expect, test } from "bun:test";
import { createManager, defaultLayout } from "../src/layout/model";

test("collapse releases a stream and restore resumes its saved cursor", () => {
  const calls: unknown[] = [];
  const manager = createManager(defaultLayout(), {
    onSubscribe: (subject, cursor) => calls.push(["sub", subject, cursor]),
    onUnsubscribe: (subject) => calls.push(["unsub", subject]),
    onFilter: () => {},
  });
  manager.state.tiles["t1"]!.cursor = 42;
  manager.collapse("t1");
  manager.restore("t1");
  expect(calls).toEqual([["unsub", "agent/fixture"], ["sub", "agent/fixture", 42]]);
});

import { createLayoutStore, SCHEMA_VERSION } from "../src/layout/persistence";
import { isPhone, previewMode } from "../src/layout/phone";

function fixture(phone = false) {
  const calls: unknown[] = [];
  const m = createManager(defaultLayout(), {
    onSubscribe: (s, c) => calls.push(["sub", s, c]),
    onUnsubscribe: s => calls.push(["unsub", s]),
    onFilter: s => calls.push(["filter", s]),
  }, phone);
  return { m, calls };
}

for (const placement of ["replace", "tab", "split_right", "split_down", { beside: "t1" }, "push"] as const) {
  test(`open placement ${JSON.stringify(placement)}`, () => {
    const { m, calls } = fixture();
    const id = m.open("artifact/example", placement, "artifact");
    expect(m.state.focused).toBe(id);
    expect(calls).toContainEqual(["sub", "artifact/example", null]);
    if (placement === "replace") {
      expect(m.state.tiles["t1"]).toBeUndefined();
      expect(calls).toContainEqual(["unsub", "agent/fixture"]);
    } else if (placement === "tab" || placement === "push") {
      expect(m.state.tree).toEqual({ type: "stack", tiles: ["t1", id], active: id });
      expect(m.state.history).toEqual(placement === "push" ? ["t1"] : []);
    } else {
      expect(m.state.tree).toEqual({ type: "split", direction: placement === "split_down" ? "vertical" : "horizontal", weights: [1, 1], children: [{ type: "stack", tiles: ["t1"], active: "t1" }, { type: "stack", tiles: [id], active: id }] });
    }
  });
}

test("closing the origin leaves the independent second tile open", () => {
  const { m } = fixture();
  const id = m.expandIntoSecond("artifact/example", "t1");
  expect(m.state.tiles[id]?.origin).toBe("t1");
  m.close("t1");
  expect(m.state.tree).toEqual({ type: "stack", tiles: [id], active: id });
  expect(m.state.tiles[id]?.subject).toBe("artifact/example");
});

test("collapse with a badge retains the subscription until close", () => {
  const { m, calls } = fixture();
  m.state.tiles["t1"]!.badge = "1 question";
  m.collapse("t1");
  expect(calls).not.toContainEqual(["unsub", "agent/fixture"]);
  m.restore("t1");
  expect(calls).not.toContainEqual(["sub", "agent/fixture", null]);
  m.collapse("t1"); m.close("t1");
  expect(calls.filter(c => JSON.stringify(c) === '["unsub","agent/fixture"]')).toHaveLength(1);
});

test("shared streams subscribe once and unsubscribe only after last owner closes", () => {
  const { m, calls } = fixture();
  const id = m.open("agent/fixture", "tab");
  m.close("t1");
  expect(calls).not.toContainEqual(["unsub", "agent/fixture"]);
  expect(calls).not.toContainEqual(["sub", "agent/fixture", null]);
  m.close(id);
  expect(calls).toContainEqual(["unsub", "agent/fixture"]);
});

test("collapse one shared stream keeps it alive, closing last expanded releases it", () => {
  const { m, calls } = fixture();
  const id = m.open("agent/fixture", "tab");
  m.collapse("t1");
  expect(calls).not.toContainEqual(["unsub", "agent/fixture"]);
  m.close(id);
  expect(calls).toContainEqual(["unsub", "agent/fixture"]);
  m.close("t1");
  expect(calls.filter(c => JSON.stringify(c) === '["unsub","agent/fixture"]')).toHaveLength(1);
});

test("focus selects a hidden stack tab and sets the inbox agent filter", () => {
  const { m, calls } = fixture();
  m.open("agent/example", "tab"); m.focus("t1");
  expect(m.state.focused).toBe("t1");
  expect(m.state.tree?.type === "stack" && m.state.tree.active).toBe("t1");
  expect(calls.at(-1)).toEqual(["filter", "agent/fixture"]);
});

test("phone defaults to push, expand pushes with origin, back returns to it", () => {
  const { m } = fixture(true);
  const tab = m.open("agent/example");
  expect(m.state.history).toEqual(["t1"]);
  const second = m.expandIntoSecond("artifact/example", "t1");
  expect(m.state.tiles[second]?.origin).toBe("t1");
  m.back(); expect(m.state.focused).toBe("t1");
  expect(m.state.tiles[tab]).toBeDefined();
});

test("desktop defaults to replace and invalid origins fail before mutation", () => {
  const { m } = fixture();
  expect(() => m.expandIntoSecond("artifact/example", "absent")).toThrow("Origin");
  expect(() => m.open("artifact/example", { beside: "absent" })).toThrow("Origin");
  expect(m.state).toEqual(defaultLayout());
  m.open("agent/example"); expect(m.state.tiles["t1"]).toBeUndefined();
});

function memory() {
  const data = new Map<string, string>();
  return { getItem: (key: string) => data.get(key) ?? null, setItem: (key: string, value: string) => { data.set(key, value); }, data };
}

test("persistence round-trip and server isolation", () => {
  const storage = memory(), a = createLayoutStore({ serverKey: "server-a.example", storage }), b = createLayoutStore({ serverKey: "server-b.example", storage });
  const { m } = fixture(); m.open("agent/example", "split_down"); m.collapse("t1");
  expect(a.save(m.state)).toBeNull();
  expect(createLayoutStore({ serverKey: "server-a.example", storage }).load().layout).toEqual(m.state);
  expect(b.load().layout).toEqual(defaultLayout());
});

for (const invalid of ["{", JSON.stringify({ version: 0, layout: defaultLayout() }), JSON.stringify({ version: SCHEMA_VERSION, layout: { ...defaultLayout(), rail: ["absent"] } }), JSON.stringify({ version: SCHEMA_VERSION, layout: { ...defaultLayout(), tree: { type: "split", children: [] } } })]) {
  test(`corrupt or obsolete data resets atomically: ${invalid.slice(0, 30)}`, () => {
    const store = createLayoutStore({ serverKey: "fixture", storage: { getItem: () => invalid, setItem() {} } });
    const loaded = store.load(); expect(loaded.layout).toEqual(defaultLayout()); expect(loaded.notice).toContain("reset");
  });
}

test("blocked storage reads and quota writes produce notices", () => {
  const store = createLayoutStore({ serverKey: "fixture", storage: { getItem() { throw new Error("denied"); }, setItem() { throw new Error("quota"); } } });
  expect(store.load().notice).toContain("reset"); expect(store.save(defaultLayout())).toContain("could not be saved");
});

test("mode overrides breakpoint in both directions; preview query is explicit", () => {
  expect(isPhone("auto", 390)).toBe(true); expect(isPhone("auto", 768)).toBe(false);
  expect(isPhone("phone", 1280)).toBe(true); expect(isPhone("desktop", 390)).toBe(false);
  expect(previewMode("?layout=phone")).toBe("phone"); expect(previewMode("?layout=desktop")).toBe("desktop"); expect(previewMode("?layout=unknown")).toBe("auto");
});
test("saved custom split weights and tab focus survive reload", () => {
  const { m } = fixture();
  const second = m.open("artifact/example", "split_right", "artifact");
  m.open("agent/example", "tab"); m.focus(second);
  if (m.state.tree?.type === "split") m.state.tree.weights = [2, 3];
  const store = createLayoutStore({ serverKey: "fixture", storage: memory() });
  store.save(m.state); expect(store.load().layout).toEqual(m.state);
});
