#!/usr/bin/env python3
"""Prototype of the full-screen flock screensaver (stage 2 of the idle gimmick).

Stage 1 (already shipped) is the subtle sidebar-bar grazing. After a longer
idle this scene takes over the whole screen: sheep graze along several fence
lines, wander off as strays, and now and then a few make a break for the screen
edge. A dog patrols, picks the worst offender (furthest stray or an active
bolter), swings to the far side of it, and drives it back to its line — classic
sheepdog geometry (the sheep flees the dog, the dog stands opposite the goal).
Any interaction wipes the scene: sheep bolt off the edges, grass recedes.

Live in a terminal (animates); piped, prints sampled frames. Needs a Nerd Font.
This is a look-and-feel prototype to eyeball before porting to src/ui/."""
import sys, time, math, random

HEAD, WOOL = chr(0xF0CC6), chr(0xF0590)   # nf-md-sheep (head), nf-md-cloud (wool)
DOG = chr(0xF0A43)                         # nf-md-dog
SPROUT, WEED = chr(0xF0E9C), chr(0xF1510)  # nf-md-sprout, nf-md-grass

random.seed(7)
W, H = 78, 22
LINES = [5, 11, 17]                        # fence rows the sheep call home
N_SHEEP = 14
GRAZE_R = 1.6        # how far above/below the home line counts as "grazing"
STRAY_R = 4.0        # beyond this from home a sheep is a stray the dog goes to fetch
SAFE_R = 2.5         # once a fetched sheep is back within this, the dog lets it be
                     # (gap with STRAY_R = hysteresis: grab far, release close)
WANDER = 1.1         # cells/sec idle drift
BOLT = 7.0           # cells/sec when breaking for the edge
RETURN = 3.2         # cells/sec fleeing the dog toward home
DOG_SPD = 9.0        # dog is faster than any sheep
FLEE_R = 5.0         # sheep starts reacting to the dog within this radius
BREAKOUT_EVERY = 10.0 # seconds between breakout events
GRASS_RECEDE = 6.0
SLEEP_AFTER_CALM = 5.0  # seconds with no strays before the dog lies down to nap
CX, CY = W / 2, H / 2   # field centre — the dog always works from outside this

class Sheep:
    def __init__(self, x, home):
        self.x, self.y, self.home = x, float(home), home
        self.vx = self.vy = 0.0
        self.facing = 1
        self.state = "graze"   # graze | stray | bolt
    def goal(self):            # where it belongs: its spot on the home line
        return self.x, self.home

sheep = []
for i in range(N_SHEEP):
    home = LINES[i % len(LINES)]
    sheep.append(Sheep(random.uniform(6, W - 6), home))

dog = {"x": W / 2, "y": 1.5, "target": None, "facing": 1,
       "mode": "guard", "sleep_t": 0.0, "calm_t": 0.0, "patrol": 0.0, "zzz_t": 0.0}

# Drifting snore: a 3-cell window the z's scroll up-and-rightward through.
ZZZ_FRAMES = ["z  ", "Zz ", "zZz", " zZ", "  z", "   "]
ZZZ_RATE = 0.22  # seconds per frame
grass = [{"x": random.randint(2, W - 3), "y": random.choice(LINES),
          "h": random.random(), "g": 0.01 + random.random() * 0.04} for _ in range(40)]
breakout_cd = BREAKOUT_EVERY

def nearest_edge_dir(x):
    return -1.0 if x < W / 2 else 1.0

def pick_target():
    """Worst offender, but *sticky*: keep driving the current sheep until it's
    back home, so the dog commits to one job instead of darting about."""
    cur = dog.get("target")
    if cur is not None and cur in sheep and (
        cur.state == "bolt" or abs(cur.y - cur.home) > SAFE_R
        or cur.x < 3 or cur.x > W - 3
    ):
        return cur            # keep driving until it's back inside the safe radius
    bolters = [s for s in sheep if s.state == "bolt"]
    pool = bolters or [s for s in sheep if abs(s.y - s.home) > STRAY_R
                       or s.x < 3 or s.x > W - 3]
    if not pool:
        return None
    return max(pool, key=lambda s: abs(s.y - s.home) + (50 if s.state == "bolt" else 0))

def step(dt, flee):
    global breakout_cd
    if flee:
        for g in grass:
            g["h"] = max(0.0, g["h"] - GRASS_RECEDE * dt)
        for s in sheep:
            d = nearest_edge_dir(s.x); s.facing = int(d)
            s.x += d * BOLT * dt
        dog["x"] += nearest_edge_dir(dog["x"]) * DOG_SPD * dt
        return
    for g in grass:
        g["h"] = min(1.8, g["h"] + g["g"] * dt)

    # trigger occasional breakouts (behaviour 2)
    breakout_cd -= dt
    if breakout_cd <= 0:
        breakout_cd = BREAKOUT_EVERY + random.uniform(-1.5, 1.5)
        for s in random.sample(sheep, k=min(3, len(sheep))):
            s.state = "bolt"

    # --- dog brain: decide whether it's napping or on duty -------------------
    tgt = pick_target()
    if tgt is None:
        dog["calm_t"] += dt
    else:
        dog["calm_t"] = 0.0
        dog["mode"] = "guard"            # any stray/breakout wakes the dog
    if dog["mode"] != "sleep" and tgt is None and dog["calm_t"] > SLEEP_AFTER_CALM:
        dog["mode"] = "sleep"
        dog["sleep_t"] = random.uniform(3.5, 7.0)
    if dog["mode"] == "sleep":
        dog["sleep_t"] -= dt
        if dog["sleep_t"] <= 0:
            dog["mode"] = "guard"
        tgt = None                        # asleep: not actively herding
    dog["target"] = tgt
    awake = dog["mode"] != "sleep"
    dog["zzz_t"] = 0.0 if awake else dog["zzz_t"] + dt

    for s in sheep:
        gx, gy = s.goal()
        near_home = abs(s.y - s.home) <= GRAZE_R
        ddog = math.hypot(s.x - dog["x"], s.y - dog["y"])
        if s is tgt and awake and ddog < FLEE_R:
            # flee the dog: move directly away from it (the dog is parked on the
            # far side of the goal, so "away" points home)
            ax, ay = s.x - dog["x"], s.y - dog["y"]
            n = math.hypot(ax, ay) or 1.0
            s.x += ax / n * RETURN * dt
            s.y += ay / n * RETURN * dt
            s.facing = 1 if ax >= 0 else -1
        elif s.state == "bolt":
            d = nearest_edge_dir(s.x); s.facing = int(d)
            s.x += d * BOLT * dt
            if s.x < 2 or s.x > W - 2:        # reached edge: now a stray to fetch
                s.state = "stray"
        else:
            # idle wander, biased gently back toward the home line
            s.x += random.uniform(-1, 1) * WANDER * dt
            s.y += (gy - s.y) * 0.6 * dt + random.uniform(-1, 1) * WANDER * dt
            s.state = "graze" if near_home else "stray"
        s.x = max(1, min(W - 2, s.x)); s.y = max(1, min(H - 2, s.y))

    if not awake:
        return                                  # napping: stays put (zZz)

    # On duty. Always work from OUTSIDE toward the centre: stand on the far side
    # of the target from the field centre, so the sheep — fleeing the dog —
    # is driven inward. With no target, patrol the perimeter rather than sit
    # in the middle.
    if tgt is not None:
        ox, oy = tgt.x - CX, tgt.y - CY
        n = math.hypot(ox, oy) or 1.0
        px, py = tgt.x + ox / n * 2.2, tgt.y + oy / n * 2.2
    else:
        dog["patrol"] += dt * 0.6
        px = CX + math.cos(dog["patrol"]) * (W / 2 - 3)
        py = CY + math.sin(dog["patrol"]) * (H / 2 - 2)
    dx, dy = px - dog["x"], py - dog["y"]
    n = math.hypot(dx, dy) or 1.0
    dog["x"] += dx / n * DOG_SPD * dt
    dog["y"] += dy / n * DOG_SPD * dt
    dog["facing"] = 1 if dx >= 0 else -1

def render():
    grid = [[" "] * W for _ in range(H)]
    for y in LINES:
        for x in range(W):
            grid[y][x] = "─"
    for g in grass:
        c = WEED if g["h"] >= 1.3 else (SPROUT if g["h"] >= 0.5 else None)
        if c and 0 <= g["y"] < H and 0 <= g["x"] < W:
            grid[g["y"]][g["x"]] = c
    for s in sheep:
        c, r = int(round(s.x)), int(round(s.y))
        if 0 <= r < H and 0 <= c < W:
            grid[r][c] = HEAD
            wx = c - s.facing
            if 0 <= wx < W:
                grid[r][wx] = WOOL
    c, r = int(round(dog["x"])), int(round(dog["y"]))
    if 0 <= r < H and 0 <= c < W:
        grid[r][c] = DOG
        if dog["mode"] == "sleep":           # animated drifting snore
            frame = ZZZ_FRAMES[int(dog["zzz_t"] / ZZZ_RATE) % len(ZZZ_FRAMES)]
            zy = r - 1
            for i, ch in enumerate(frame):
                zx = c + 1 + i
                if ch != " " and 0 <= zy < H and 0 <= zx < W:
                    grid[zy][zx] = ch
    return "\n".join("".join(row) for row in grid)

live = sys.stdout.isatty()
if live:
    print("\x1b[2J", end="")
for frame in range(700):
    flee = frame >= 685
    step(0.08, flee)
    if live:
        sys.stdout.write("\x1b[H" + render()); sys.stdout.flush(); time.sleep(0.08)
    elif frame in (12, 80, 150, 220, 300, 690):
        print(f"\n--- t={frame*0.08:.1f}s {'WIPE' if flee else ''} ---")
        print(render())
