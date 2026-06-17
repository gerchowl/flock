#!/usr/bin/env python3
"""Preview the flock-idle gimmick (mirrors src/ui/sheep.rs).

Grass grows on two separator bars. Sheep wander IN from the left/right edges,
roam the field — moving diagonally / Manhattan-style across the gap to reach
grass on either bar — wait for ripe tufts, the closest sheep wins one, crops it,
and after a few patches ambles back off the nearest SIDE. They route around each
other. Run in a terminal for live animation; piped, prints sampled frames.
Needs a Nerd Font."""
import sys, time, random

HEAD, WOOL, SPROUT, WEED = (chr(c) for c in (0xF0CC6, 0xF0590, 0xF0E9C, 0xF1510))
W = 50
MAX, SPROUT_AT, RIPE, CROP = 2.0, 0.5, 1.4, 0.3
WALK, CLIMB, EAT, FLEE = 3.0, 2.0, 0.9, 42.0
ARRIVE, GAP, RETIRE_AT, COOLDOWN = 0.6, 2.0, 4, 1.5
BAR_Y = [3, 9]
XMIN, XMAX = 0, W - 1
CAP = sum(max(1, min(8, int(W * 0.30 / 2))) for _ in BAR_Y)
random.seed(5)

def new_bar(y):
    tufts, x = [], 1
    while x < W - 1:
        tufts.append({"x": x, "h": random.random()*0.6, "g": 0.02 + random.random()*0.10})
        x += 2 + random.randint(0, 4)
    return {"y": y, "tufts": tufts}

bars = [new_bar(y) for y in BAR_Y]
sheep, age, cd = [], 0.0, 0.0

def glyph(h):
    return WEED if h >= RIPE else (SPROUT if h >= SPROUT_AT else None)

def tick(dt, flee):
    global age, cd
    if not flee: age += dt
    ramp = min(2.0, 1 + age/30)
    if not flee:
        for b in bars:
            for t in b["tufts"]: t["h"] = min(MAX, t["h"] + t["g"]*ramp*dt)
    if flee:
        for s in sheep:
            d = -1 if s["x"] < W/2 else 1; s["f"]=d; s["x"] += d*FLEE*dt
        sheep[:] = [s for s in sheep if XMIN-1 <= s["x"] <= XMAX+1]
        return
    # spawn
    cd -= dt
    if len(sheep) < CAP and cd <= 0 and any(t["h"]>.15 for b in bars for t in b["tufts"]):
        left = random.random() < .5; bi = random.randrange(len(bars))
        ex, ey = (XMIN if left else XMAX), bars[bi]["y"]
        if not any(abs(s["y"]-ey)<1 and abs(s["x"]-ex)<GAP for s in sheep):
            sheep.append({"x":float(ex),"y":float(ey),"f":1 if left else -1,"tg":None,"st":"roam","eaten":0})
            cd = COOLDOWN + random.random()
        else:
            cd = 0.4
    # claim closest-wins (Manhattan)
    eating = {s["tg"] for s in sheep if s["st"][0]=="e"}
    ripe = [(bi,t["x"],b["y"]) for bi,b in enumerate(bars) for t in b["tufts"] if t["h"]>=RIPE and (bi,t["x"]) not in eating]
    for s in sheep:
        if s["st"]=="roam": s["tg"]=None
    free=[s for s in sheep if s["st"]=="roam"]
    while ripe and free:
        best=None
        for s in free:
            for (bi,tx,ty) in ripe:
                d=abs(s["x"]-tx)+abs(s["y"]-ty)
                if best is None or d<best[3]: best=(s,bi,tx,d,ty)
        s,bi,tx,_,ty=best; s["tg"]=(bi,tx); free.remove(s); ripe.remove((bi,tx,ty))
    # move
    cells=[(i,s["x"],s["y"]) for i,s in enumerate(sheep)]
    occ=lambda i,nx,ny:any(j!=i and round(ox)==round(nx) and round(oy)==round(ny) for j,ox,oy in cells)
    for i,s in enumerate(sheep):
        st=s["st"]
        if st[0]=="e":
            bi,tx=s["tg"]; t=next((t for t in bars[bi]["tufts"] if t["x"]==tx),None)
            if t: t["h"]=max(0,t["h"]-EAT*dt)
            if t is None or t["h"]<CROP:
                s["eaten"]+=1; s["st"]="leave" if s["eaten"]>=RETIRE_AT else "roam"; s["tg"]=None
        elif st=="leave":
            d=-1 if s["x"]<W/2 else 1; s["f"]=d; s["x"]+=d*WALK*dt
        else:
            if s["tg"] is None: continue
            bi,tx=s["tg"]; gx,gy=tx,bars[bi]["y"]; x,y=s["x"],s["y"]
            if abs(x-gx)<=ARRIVE and abs(y-gy)<=ARRIVE:
                s["x"],s["y"]=float(gx),float(gy); s["st"]=("eat",); s["tg"]=(bi,tx); continue
            sx=max(-WALK*dt,min(WALK*dt,gx-x)); sy=max(-CLIMB*dt,min(CLIMB*dt,gy-y))
            if sx!=0: s["f"]=1 if sx>0 else -1
            if not occ(i,x+sx,y+sy): s["x"]+=sx; s["y"]+=sy
            elif sy!=0 and not occ(i,x,y+sy): s["y"]+=sy
            elif sx!=0 and not occ(i,x+sx,y): s["x"]+=sx
    sheep[:] = [s for s in sheep if not (s["st"]=="leave" and (s["x"]<XMIN-1 or s["x"]>XMAX+1))]

def render():
    H = BAR_Y[-1] + 3
    grid=[[" "]*W for _ in range(H)]
    for b in bars:
        for x in range(W): grid[b["y"]][x]="─"
        for t in b["tufts"]:
            g=glyph(t["h"])
            if g: grid[b["y"]][t["x"]]=g
    for s in sheep:
        c=round(s["x"]); r=round(s["y"])
        if 0<=c<W and 0<=r<H:
            grid[r][c]=HEAD
            wx=c-s["f"]
            if 0<=wx<W: grid[r][wx]=WOOL
    return "\n".join("".join(r) for r in grid)

live=sys.stdout.isatty()
if live: print(f"flock idle preview (2 bars, enter/exit from the sides) — Ctrl-C to stop\n")
for frame in range(560):
    flee = frame>=550
    tick(0.1, flee)
    if live:
        sys.stdout.write("\x1b[H\x1b[J"+render()); sys.stdout.flush(); time.sleep(0.1)
    elif frame in (10, 140, 280, 420, 551, 555):
        print(f"--- t={frame/10:.1f}s {'FLEE' if flee else ''} ---"); print(render())
