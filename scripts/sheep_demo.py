#!/usr/bin/env python3
"""Preview the flock-idle gimmick (mirrors src/ui/sheep.rs).

Grass grows slowly; sheep drop in from above/below the line, land on it, wait
for a tuft to ripen, then the closest sheep wins it; they crop it, move on, and
after a few patches step back off the line and leave while fresh ones arrive.
Per-bar sheep cap scales with width (~30% occupancy). Run in a terminal for
live animation; piped, it prints sampled 3-row frames (above / line / below).
Needs a Nerd Font."""
import sys, time, random

HEAD, WOOL, SPROUT, WEED = (chr(c) for c in (0xF0CC6, 0xF0590, 0xF0E9C, 0xF1510))
W = 54
MAX, SPROUT_AT, RIPE, CROP = 2.0, 0.5, 1.4, 0.3
WALK, EAT, FLEE = 3.0, 0.9, 42.0
ENTER, LEAVE = 0.5, 0.5
RETIRE_AT, COOLDOWN = 4, 2.0
CAP = max(1, min(8, int(W * 0.30 / 2)))
random.seed(7)

tufts, x = [], 1
while x < W - 1:
    tufts.append({"x": x, "h": random.random()*0.6, "g": 0.02 + random.random()*0.10})
    x += 2 + random.randint(0, 4)
sheep, cd, age = [], random.random()*COOLDOWN, 0.0

def glyph(h):
    return WEED if h >= RIPE else (SPROUT if h >= SPROUT_AT else None)

def tick(dt, flee):
    global cd, age
    if not flee:
        age += dt
    ramp = min(2.0, 1 + age/30)
    if not flee:
        for t in tufts:
            t["h"] = min(MAX, t["h"] + t["g"]*ramp*dt)
        cd -= dt
        if len(sheep) < CAP and cd <= 0 and any(t["h"] > 0.15 for t in tufts):
            rx = random.random()
            sheep.append({"x": rx*(W-1), "f": 1 if rx < .5 else -1,
                          "yo": -1 if random.random() < .5 else 1, "tm": ENTER,
                          "st": "enter", "eaten": 0})
            cd = COOLDOWN + random.random()
    if flee:
        for s in sheep:
            d = -1 if s["x"] < W/2 else 1; s["f"]=d; s["yo"]=0; s["x"] += d*FLEE*dt
        return
    eating = {s.get("tx") for s in sheep if s["st"]=="eat"}
    ripe = [t["x"] for t in tufts if t["h"] >= RIPE and t["x"] not in eating]
    free = [s for s in sheep if s["st"] in ("peek","walk")]
    claims = {}
    while ripe and free:
        best=None
        for s in free:
            for tx in ripe:
                d=abs(s["x"]-tx)
                if best is None or d<best[2]: best=(s,tx,d)
        s,tx,_=best; claims[id(s)]=tx; free.remove(s); ripe.remove(tx)
    for s in sheep:
        st=s["st"]
        if st=="enter":
            s["tm"]-=dt
            if s["tm"]<=0: s["yo"]=0; s["st"]="peek"
        elif st=="leave":
            s["tm"]-=dt
        elif st=="eat":
            s["x"]=float(s["tx"]); t=next((t for t in tufts if t["x"]==s["tx"]),None)
            if t: t["h"]=max(0,t["h"]-EAT*dt)
            if t is None or t["h"]<CROP:
                s["eaten"]+=1
                if s["eaten"]>=RETIRE_AT: s["st"]="leave"; s["yo"]=-1 if s["tx"]%2==0 else 1; s["tm"]=LEAVE
                else: s["st"]="peek"
        elif st in ("peek","walk"):
            tx=claims.get(id(s))
            if tx is None: s["st"]="peek"
            elif abs(s["x"]-tx)>0.6: s["f"]=1 if tx>s["x"] else -1; s["x"]+=s["f"]*WALK*dt; s["st"]="walk"
            else: s["x"]=float(tx); s["st"]="eat"; s["tx"]=tx
    sheep[:] = [s for s in sheep if not (s["st"]=="leave" and s["tm"]<=0)]

def render():
    rows = {-1: [" "]*W, 0: ["─"]*W, 1: [" "]*W}
    for t in tufts:
        g=glyph(t["h"])
        if g: rows[0][t["x"]]=g
    for s in sheep:
        c=round(s["x"])
        if 0<=c<W:
            r=rows[s["yo"]]; r[c]=HEAD
            wx=c-s["f"]
            if 0<=wx<W: r[wx]=WOOL
    return ["".join(rows[-1]), "".join(rows[0]), "".join(rows[1])]

live=sys.stdout.isatty()
if live: print(f"flock idle preview (cap {CAP}) — Ctrl-C to stop; last second = flee\n")
for frame in range(420):
    flee = frame>=410
    tick(0.1, flee)
    if live:
        a,l,b=render(); sys.stdout.write("\x1b[2K"+a+"\n\x1b[2K"+l+"\n\x1b[2K"+b+"\x1b[2A\r"); sys.stdout.flush(); time.sleep(0.1)
    elif frame % 40 == 0 or (flee and frame % 3 == 0):
        a,l,b=render()
        print(f"t={frame/10:5.1f}s {'FLEE' if flee else '    '}  {a}")
        print(f"                  {l}")
        print(f"                  {b}")
if live: print("\n\n")
