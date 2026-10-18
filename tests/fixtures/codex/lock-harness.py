import os
import sys
import tty
from pathlib import Path

base = Path(__file__).parent.parent
tty.setraw(0)
log = base / 'startup-input'
log.write_bytes(b'')

def paint(text):
    sys.stdout.write('\x1b[2J\x1b[H' + text.replace('\n', '\r\n'))
    sys.stdout.flush()

paint((base / 'screen').read_text())
while True:
    byte = os.read(0, 1)
    if not byte:
        break
    with log.open('ab') as out:
        out.write(byte)
    if byte in (b'r', b'f', b'u') and not (base / 'stay-locked').exists():
        paint("OpenAI Codex\n› Ask Codex to do anything\n  ? for shortcuts\n")
