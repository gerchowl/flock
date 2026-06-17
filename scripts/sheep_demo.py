#!/usr/bin/env python3
"""Preview the flock-idle gimmick (mirrors src/ui/sheep.rs).

Each separator bar is its own strip. Grass grows slowly; a sheep walks IN from
the left or right SIDE of a bar only when there's spare ripe grass for it, heads
straight to the nearest ripe tuft the closest sheep isn't already claiming,
crops it, and after a few patches ambles off the side. No bar-to-bar crossing;
no loitering at the edges. Run in a terminal for live animation; piped, prints
sampled frames. Needs a Nerd Font."""
import sys, time, random

HEAD, WOOL, SPROUT, WEED = (chr(c) for c in (0xF0CC6, 0xF0590, 0xF0E9C, 0xF1510))
W = 50
MAX, SPROUT_AT, RIPE, CROP = 2.0, 0.5, 1.4, 0.3
WALK, EAT, FLEE, ARRIVE = 3.0, 0.9, 42.0, 0.6
RETIRE_AT, COOLDOWN = 4, 1.5
BAR_Y = [3, 9]
CAP = max(1, min(8, int(W * 0.30 / 2)))
random.seed(5)

def new_lane(y):
    tufts, x = [], 1
    while x < W - 1:
        tufts.append({"x": x, "h": random.random()*0.6, "g": 0.02 + random.random()*0.10})
        x += 2 + random.randint(0, 4)
    return {"y": y, "tufts": tufts, "sheep": [], "cd": random.random()*COOLDOWN, "left": random.random() < .5}

lanes = [new_lane(y) for y in BAR_Y]
age = 0.0

def glyph(h):
    return WEED if h >= RIPE else (SPROUT if h >= SPROUT_AT else None)

def tick(dt, flee):
    global age
    if not flee: age += dt
    ramp = min(2.0, 1 + age/30)
    for L in lanes:
        if not flee:
            for t in L["tufts"]: t["h"] = min(MAX, t["h"] + t["g"]*ramp*dt)
        if flee:
            for s in L["sheep"]:
                d = -1 if s["x"] < W/2 else 1; s["f"]=d; s["x"] += d*FLEE*dt
            L["sheep"] = [s for s in L["sheep"] if -1 <= s["x"] <= W]
            continue
        # spawn only when spare ripe grass
        L["cd"] -= dt
        ripe_n = sum(1 for t in L["tufts"] if t["h"] >= RIPE)
        if L["cd"] <= 0 and len(L["sheep"]) < CAP and ripe_n > len(L["sheep"]):
            left = L["left"]; ex = 0 if left else W-1
            L["sheep"].append({"x": float(ex), "f": 1 if left else -1, "st": "walk", "tx": ex, "eaten": 0})
            L["left"] = not left; L["cd"] = COOLDOWN + random.random()
        # claim closest-wins
        eating = {s["tx"] for s in L["sheep"] if s["st"]=="eat"}
        ripe = [t["x"] for t in L["tufts"] if t["h"] >= RIPE and t["x"] not in eating]
        free = [s for s in L["sheep"] if s["st"]=="walk"]
        claims = {}
        while ripe and free:
            best=None
            for s in free:
                for tx in ripe:
                    d=abs(s["x"]-tx)
                    if best is None or d<best[2]: best=(s,tx,d)
            s,tx,_=best; claims[id(s)]=tx; free.remove(s); ripe.remove(tx)
        cells=[(i,s["x"]) for i,s in enumerate(L["sheep"])]
        for i,s in enumerate(L["sheep"]):
            if s["st"]=="eat":
                s["x"]=float(s["tx"]); t=next((t for t in L["tufts"] if t["x"]==s["tx"]),None)
                if t: t["h"]=max(0,t["h"]-EAT*dt)
                if t is None or t["h"]<CROP:
                    s["eaten"]+=1; s["st"]="leave" if s["eaten"]>=RETIRE_AT else "walk"
            elif s["st"]=="leave":
                d=-1 if s["x"]<W/2 else 1; s["f"]=d; s["x"]+=d*WALK*dt
            else:
                tx=claims.get(id(s))
                if tx is None: s["st"]="leave"; continue
                if abs(s["x"]-tx)<=ARRIVE: s["x"]=float(tx); s["st"]="eat"; s["tx"]=tx; continue
                fc = 1 if tx>s["x"] else -1; nx=s["x"]+fc*WALK*dt; s["f"]=fc
                if not any(j!=i and round(ox)==round(nx) for j,ox in cells): s["x"]=nx
        L["sheep"] = [s for s in L["sheep"] if not (s["st"]=="leave" and (s["x"]<-1 or s["x"]>W))]

def render():
    H = BAR_Y[-1] + 3
    grid=[[" "]*W for _ in range(H)]
    for L in lanes:
        for x in range(W): grid[L["y"]][x]="─"
        for t in L["tufts"]:
            g=glyph(t["h"])
            if g: grid[L["y"]][t["x"]]=g
        for s in L["sheep"]:
            c=round(s["x"])
            if 0<=c<W:
                grid[L["y"]][c]=HEAD
                wx=c-s["f"]
                if 0<=wx<W: grid[L["y"]][wx]=WOOL
    return "\n".join("".join(r) for r in grid)

live=sys.stdout.isatty()
if live: print("flock idle preview — independent bars, side entry/exit — Ctrl-C\n")
for frame in range(560):
    flee = frame>=550
    tick(0.1, flee)
    if live:
        sys.stdout.write("\x1b[H\x1b[J"+render()); sys.stdout.flush(); time.sleep(0.1)
    elif frame in (10, 140, 280, 420, 551, 556):
        print(f"--- t={frame/10:.1f}s {'FLEE' if flee else ''} ---"); print(render())
