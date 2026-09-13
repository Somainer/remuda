# Independently written Grok 1.0.30 spike apparatus; see README.md.
import os,pty,sys
from pathlib import Path
log=Path('/tmp/remuda-grokspike/tui/raw.ansi').open('ab',buffering=0)
def read(fd):
 b=os.read(fd,65536);log.write(b);return b
pty.spawn(['python3','/tmp/remuda-grokspike/tui/child.py'],read)
