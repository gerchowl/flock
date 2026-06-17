#!/usr/bin/env python3
"""Preview the flock-idle gimmick (mirrors src/ui/sheep.rs).

Each bar is its own strip. Grass grows slowly; a sheep walks in from a SIDE only
when there's spare ripe grass, heads for the nearest ripe tuft (closest wins),
and may stray +-2 rows around its bar to step around others before returning to
the line to graze; after a few patches it ambles off the side. On interaction
the flock bolts off the sides AND the grass withers away. Run in a terminal for
live animation; piped, prints sampled frames. Needs a Nerd Font."""
import sys, time, random

HEAD, WOOL, SPROUT, WEED = (chr(c) for c in (0xF0CC6, 0xF0590, 0xF0E9C, 0xF1510))
W = 50
MAX, SPROUT_AT, RIPE, CROP = 2.0, 0.5, 1.4, 0.3
WALK, CLIMB, BAND, EAT, FLEE, RECEDE, ARRIVE = 3.0, 2.5, 2, 0.9, 42.0, 5.0, 0.6
RETIRE_AT, COOLDOWN = 4, 1.5
BAR_Y = [4, 11]
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
        line = L["y"]
        if flee:
            for t in L["tufts"]: t["h"] = max(0, t["h"] - RECEDE*dt)
            for s in L["sheep"]:
                d = -1 if s["x"] < W/2 else 1; s["f"]=d; s["x"] += d*FLEE*dt
            L["sheep"] = [s for s in L["sheep"] if -1 <= s["x"] <= W]
            continue
        for t in L["tufts"]: t["h"] = min(MAX, t["h"] + t["g"]*ramp*dt)
        L["cd"] -= dt
        ripe_n = sum(1 for t in L["tufts"] if t["h"] >= RIPE)
        if L["cd"] <= 0 and len(L["sheep"]) < CAP and ripe_n > len(L["sheep"]):
            left = L["left"]; ex = 0 if left else W-1
            L["sheep"].append({"x": float(ex), "y": float(line), "f": 1 if left else -1, "st": "walk", "tx": ex, "eaten": 0})
            L["left"] = not left; L["cd"] = COOLDOWN + random.random()
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
        cells=[(i,s["x"],s["y"]) for i,s in enumerate(L["sheep"])]
        occ=lambda i,nx,ny:any(j!=i and round(ox)==round(nx) and round(oy)==round(ny) for j,ox,oy in cells)
        for i,s in enumerate(L["sheep"]):
            if s["st"]=="eat":
                s["x"]=float(s["tx"]); s["y"]=float(line); t=next((t for t in L["tufts"] if t["x"]==s["tx"]),None)
                if t: t["h"]=max(0,t["h"]-EAT*dt)
                if t is None or t["h"]<CROP:
                    s["eaten"]+=1; s["st"]="leave" if s["eaten"]>=RETIRE_AT else "walk"
            elif s["st"]=="leave":
                d=-1 if s["x"]<W/2 else 1; s["f"]=d; s["x"]+=d*WALK*dt
            else:
                tx=claims.get(id(s))
                if tx is None: s["st"]="leave"; continue
                x,y=s["x"],s["y"]
                if abs(x-tx)<=ARRIVE and abs(y-line)<=ARRIVE: s["x"]=float(tx); s["y"]=float(line); s["st"]="eat"; s["tx"]=tx; continue
                fc = 1 if tx>x else -1; sx=max(-WALK*dt,min(WALK*dt,tx-x)); s["f"]=fc
                toline = max(-CLIMB*dt,min(CLIMB*dt,line-y))
                up=max(y-CLIMB*dt, line-BAND); dn=min(y+CLIMB*dt, line+BAND)
                for nx,ny in [(x+sx,y+toline),(x+sx,up),(x+sx,dn),(x,up),(x,dn)]:
                    if not occ(i,nx,ny): s["x"]=nx; s["y"]=ny; break
        L["sheep"] = [s for s in L["sheep"] if not (s["st"]=="leave" and (s["x"]<-1 or s["x"]>W))]

def render():
    H = BAR_Y[-1] + 4
    grid=[[" "]*W for _ in range(H)]
    for L in lanes:
        for x in range(W): grid[L["y"]][x]="─"
        for t in L["tufts"]:
            g=glyph(t["h"])
            if g: grid[L["y"]][t["x"]]=g
        for s in L["sheep"]:
            c=round(s["x"]); r=round(s["y"])
            if 0<=c<W and 0<=r<H:
                grid[r][c]=HEAD
                wx=c-s["f"]
                if 0<=wx<W: grid[r][wx]=WOOL
    return "\n".join("".join(r) for r in grid)

live=sys.stdout.isatty()
if live: print("flock idle preview — independent bars, ±2 band, grass wipes on action\n")
for frame in range(560):
    flee = frame>=545
    tick(0.1, flee)
    if live:
        sys.stdout.write("\x1b[H\x1b[J"+render()); sys.stdout.flush(); time.sleep(0.1)
    elif frame in (10, 200, 380, 540, 546, 552):
        print(f"--- t={frame/10:.1f}s {'FLEE' if flee else ''} ---"); print(render())
