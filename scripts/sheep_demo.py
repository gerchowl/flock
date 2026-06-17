#!/usr/bin/env python3
"""Preview the flock-idle gimmick (mirrors src/ui/sheep.rs).

Grass grows organically; sheep peek at the edges and only commit once a tuft is
ripe; the closest sheep wins a tuft; they crop it, move on, and retire after a
few patches while fresh ones wander in; growth ramps up over the idle spell.
Two lanes so they don't crowd. Run in a terminal for live animation; piped, it
prints sampled frames. Needs a Nerd Font."""
import sys, time, random

HEAD, WOOL, SPROUT, WEED = (chr(c) for c in (0xF0CC6, 0xF0590, 0xF0E9C, 0xF1510))
W = 54
MAX, SPROUT_AT, RIPE, CROP = 2.0, 0.5, 1.4, 0.3
WALK, EAT, FLEE = 3.0, 0.9, 42.0
RETIRE_AT, PER_LANE, COOLDOWN = 4, 2, 2.0
random.seed(7)

def new_lane():
    tufts, x = [], 1
    while x < W - 1:
        tufts.append({"x": x, "h": random.random() * 0.6, "g": 0.05 + random.random() * 0.28})
        x += 2 + random.randint(0, 4)
    return {"tufts": tufts, "sheep": [], "cd": random.random() * COOLDOWN, "left": random.random() < 0.5}

lanes = [new_lane(), new_lane()]
age = 0.0

def glyph(h):
    return WEED if h >= RIPE else (SPROUT if h >= SPROUT_AT else None)

def tick(dt, flee):
    global age
    if not flee:
        age += dt
    ramp = min(3.0, 1 + age / 18 * 2)
    for L in lanes:
        if not flee:
            for t in L["tufts"]:
                t["h"] = min(MAX, t["h"] + t["g"] * ramp * dt)
        L["cd"] -= dt
        if not flee and len(L["sheep"]) < PER_LANE and L["cd"] <= 0 and any(t["h"] > 0.15 for t in L["tufts"]):
            L["sheep"].append({"x": float(0 if L["left"] else W - 1),
                               "f": 1 if L["left"] else -1, "st": "peek", "tx": None, "eaten": 0})
            L["left"] = not L["left"]
            L["cd"] = COOLDOWN + random.random()
        if flee:
            for s in L["sheep"]:
                d = -1 if s["x"] < W / 2 else 1
                s["f"] = d
                s["x"] += d * FLEE * dt
            continue
        eating = {s["tx"] for s in L["sheep"] if s["st"] == "eat"}
        ripe = [t["x"] for t in L["tufts"] if t["h"] >= RIPE and t["x"] not in eating]
        free = [s for s in L["sheep"] if s["st"] in ("peek", "walk")]
        claims = {}
        while ripe and free:
            best = None
            for s in free:
                for tx in ripe:
                    d = abs(s["x"] - tx)
                    if best is None or d < best[2]:
                        best = (s, tx, d)
            s, tx, _ = best
            claims[id(s)] = tx
            free.remove(s)
            ripe.remove(tx)
        for s in L["sheep"]:
            if s["st"] == "eat":
                s["x"] = float(s["tx"])
                t = next((t for t in L["tufts"] if t["x"] == s["tx"]), None)
                if t:
                    t["h"] = max(0.0, t["h"] - EAT * dt)
                if t is None or t["h"] < CROP:
                    s["eaten"] += 1
                    s["st"] = "retire" if s["eaten"] >= RETIRE_AT else "peek"
            elif s["st"] in ("peek", "walk"):
                tx = claims.get(id(s))
                if tx is None:
                    s["st"] = "peek"
                elif abs(s["x"] - tx) > 0.6:
                    s["f"] = 1 if tx > s["x"] else -1
                    s["x"] += s["f"] * WALK * dt
                    s["st"] = "walk"
                else:
                    s["x"] = float(tx)
                    s["st"] = "eat"
                    s["tx"] = tx
            elif s["st"] == "retire":
                d = -1 if s["x"] < W / 2 else 1
                s["f"] = d
                s["x"] += d * WALK * dt
        L["sheep"] = [s for s in L["sheep"] if not (s["st"] == "retire" and (s["x"] < -1 or s["x"] > W))]

def render(L):
    row = ["─"] * W
    for t in L["tufts"]:
        g = glyph(t["h"])
        if g:
            row[t["x"]] = g
    for s in L["sheep"]:
        c = round(s["x"])
        if 0 <= c < W:
            row[c] = HEAD
            wx = c - s["f"]
            if 0 <= wx < W:
                row[wx] = WOOL
    return "".join(row)

live = sys.stdout.isatty()
if live:
    print("flock idle preview — two lanes, Ctrl-C to stop (last second = flee)\n")
for frame in range(280):
    flee = frame >= 270
    tick(0.1, flee)
    if live:
        sys.stdout.write("\x1b[2K" + render(lanes[0]) + "\n\x1b[2K" + render(lanes[1]) + "\x1b[1A\r")
        sys.stdout.flush()
        time.sleep(0.1)
    elif frame % 25 == 0 or flee:
        print(f"t={frame/10:5.1f}s {'FLEE' if flee else '    '}  {render(lanes[0])}")
        print(f"                  {render(lanes[1])}")
if live:
    print("\n")
