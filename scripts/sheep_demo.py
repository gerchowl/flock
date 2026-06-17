#!/usr/bin/env python3
"""Preview the flock-idle gimmick (mirrors src/ui/sheep.rs).

Two separator bars. Grass grows slowly; sheep walk in from above/below, descend
onto a bar, wait for a tuft to ripen, then the closest sheep wins it. They yield
to avoid bumping; a sheep stuck behind another hops up/down to the other bar.
After a few patches they step back off and leave. Run in a terminal for live
animation; piped, it prints sampled multi-row frames. Needs a Nerd Font."""
import sys, time, random

HEAD, WOOL, SPROUT, WEED = (chr(c) for c in (0xF0CC6, 0xF0590, 0xF0E9C, 0xF1510))
W = 50
MAX, SPROUT_AT, RIPE, CROP = 2.0, 0.5, 1.4, 0.3
WALK, EAT, FLEE, VSP = 3.0, 0.9, 42.0, 4.0
ROWS, GAP, BLOCK = 2.5, 2.0, 0.7
RETIRE_AT, COOLDOWN = 4, 2.0
CAP = max(1, min(8, int(W * 0.30 / 2)))
LANE_Y = [3, 9]
random.seed(3)

def new_lane(y):
    tufts, x = [], 1
    while x < W - 1:
        tufts.append({"x": x, "h": random.random()*0.6, "g": 0.02 + random.random()*0.10})
        x += 2 + random.randint(0, 4)
    return {"y": y, "tufts": tufts, "sheep": [], "cd": random.random()*COOLDOWN}

lanes = [new_lane(y) for y in LANE_Y]
age = 0.0

def glyph(h):
    return WEED if h >= RIPE else (SPROUT if h >= SPROUT_AT else None)

def tick(dt, flee):
    global age
    if not flee: age += dt
    ramp = min(2.0, 1 + age/30)
    migrants = []
    for li, L in enumerate(lanes):
        if not flee:
            for t in L["tufts"]:
                t["h"] = min(MAX, t["h"] + t["g"]*ramp*dt)
            L["cd"] -= dt
            if len(L["sheep"]) < CAP and L["cd"] <= 0 and any(t["h"] > .15 for t in L["tufts"]):
                rx = random.random(); above = random.random() < .5
                L["sheep"].append({"x": rx*(W-1), "f": 1 if rx<.5 else -1,
                    "y": -ROWS if above else ROWS, "vd": -1 if above else 1,
                    "blk": 0.0, "mig": None, "st": "enter", "tx": None, "eaten": 0})
                L["cd"] = COOLDOWN + random.random()
        if flee:
            for s in L["sheep"]:
                d = -1 if s["x"] < W/2 else 1; s["f"]=d; s["y"]=0; s["x"] += d*FLEE*dt
            continue
        eating = {s.get("tx") for s in L["sheep"] if s["st"]=="eat"}
        ripe = [t["x"] for t in L["tufts"] if t["h"] >= RIPE and t["x"] not in eating]
        free = [s for s in L["sheep"] if s["st"] in ("peek","walk")]
        claims = {}
        while ripe and free:
            best=None
            for s in free:
                for tx in ripe:
                    d=abs(s["x"]-tx)
                    if best is None or d<best[2]: best=(s,tx,d)
            s,tx,_=best; claims[id(s)]=tx; free.remove(s); ripe.remove(tx)
        occ = [(i, s["x"]) for i,s in enumerate(L["sheep"]) if abs(s["y"]) < .5]
        for i,s in enumerate(L["sheep"]):
            st=s["st"]
            if st=="enter":
                s["y"] -= (1 if s["y"]>0 else -1)*VSP*dt
                if abs(s["y"])<=.5: s["y"]=0; s["st"]="peek"
            elif st=="leave":
                s["y"] += s["vd"]*VSP*dt
            elif st=="eat":
                s["x"]=float(s["tx"]); t=next((t for t in L["tufts"] if t["x"]==s["tx"]),None)
                if t: t["h"]=max(0,t["h"]-EAT*dt)
                if t is None or t["h"]<CROP:
                    s["eaten"]+=1
                    if s["eaten"]>=RETIRE_AT: s["st"]="leave"; s["vd"]=-1 if s["tx"]%2==0 else 1
                    else: s["st"]="peek"
            else:
                tx=claims.get(id(s))
                if tx is None: s["st"]="peek"; s["blk"]=0.0; continue
                if abs(s["x"]-tx)<=.6: s["x"]=float(tx); s["st"]="eat"; s["tx"]=tx; s["blk"]=0.0; continue
                fc = 1 if tx>s["x"] else -1; nx = s["x"]+fc*WALK*dt; s["f"]=fc
                blk = any(j!=i and (ox-s["x"])*fc>0 and abs(nx-ox)<GAP for j,ox in occ)
                if blk:
                    s["blk"]+=dt; s["st"]="walk"
                    if s["blk"]>=BLOCK and len(lanes)>1:
                        down = li+1 < len(lanes) and (li==0 or tx%2==0)
                        s["vd"]=1 if down else -1; s["mig"]=li+1 if down else li-1
                        s["st"]="leave"; s["blk"]=0.0
                else:
                    s["x"]=nx; s["st"]="walk"; s["blk"]=0.0
    # reap + migrate
    for li,L in enumerate(lanes):
        keep=[]
        for s in L["sheep"]:
            if s["st"]=="leave" and abs(s["y"])>ROWS:
                if s["mig"] is not None: migrants.append((s["mig"], s))
            else: keep.append(s)
        L["sheep"]=keep
    for tgt,s in migrants:
        if 0<=tgt<len(lanes) and len(lanes[tgt]["sheep"])<CAP:
            s["st"]="enter"; s["y"]=-s["vd"]*ROWS; s["mig"]=None; s["blk"]=0.0
            lanes[tgt]["sheep"].append(s)

def render():
    H = LANE_Y[-1] + 3
    grid = [[" "]*W for _ in range(H)]
    for L in lanes:
        for x in range(W): grid[L["y"]][x] = "─"
    for L in lanes:
        for t in L["tufts"]:
            g=glyph(t["h"])
            if g: grid[L["y"]][t["x"]]=g
        for s in L["sheep"]:
            c=round(s["x"]); r=L["y"]+round(s["y"])
            if 0<=c<W and 0<=r<H:
                grid[r][c]=HEAD
                wx=c-s["f"]
                if 0<=wx<W: grid[r][wx]=WOOL
    return "\n".join("".join(row) for row in grid)

live=sys.stdout.isatty()
if live:
    H = LANE_Y[-1]+3
    print(f"flock idle preview (2 bars, cap {CAP}/bar) — Ctrl-C to stop\n")
for frame in range(520):
    flee = frame>=510
    tick(0.1, flee)
    if live:
        sys.stdout.write("\x1b[H\x1b[J"+render()); sys.stdout.flush(); time.sleep(0.1)
    elif frame in (0, 120, 260, 400, 511, 514):
        print(f"--- t={frame/10:.1f}s {'FLEE' if flee else ''} ---")
        print(render())
