import os
from pathlib import Path
import sys

base = Path(sys.argv[1])


def paint(stage):
    if stage in ('start', 'send'):
        body = (base / (stage + '.txt')).read_text()
    elif stage == 'draft':
        body = '› real ⠁draft⢀\n  ? for shortcuts\n'
    elif stage == 'braille':
        body = '› ⠁⢀⣿\n  ? for shortcuts\n'
    elif stage == 'working':
        body = '• Working (1s • esc to interrupt)\n› \n  ? for shortcuts\n'
    else:
        raise RuntimeError(stage)
    sys.stdout.write('\x1b[3J\x1b[2J\x1b[HOpenAI Codex\nFRAME-' + stage + '\n' + body)
    sys.stdout.flush()


line = bytearray()
with (base / 'bytes').open('ab', buffering=0) as received:
    paint('start')
    while True:
        byte = os.read(0, 1)
        if not byte:
            break
        received.write(byte)
        if byte != b'\n':
            line.extend(byte)
            continue
        text = line.decode('utf-8')
        line.clear()
        if text in ('send', 'draft', 'braille'):
            paint(text)
        elif text.startswith('Read '):
            with (base / 'accepted').open('a') as accepted:
                accepted.write(text + '\n')
            paint('working')
        else:
            raise RuntimeError('unexpected input: ' + repr(text))
