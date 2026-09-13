# Independently written Grok 1.0.30 spike apparatus; see README.md.
import os,fcntl,termios,struct
fcntl.ioctl(0,termios.TIOCSWINSZ,struct.pack("HHHH",54,105,0,0))
os.execvp("sh",["sh","/tmp/remuda-grokspike/tui/launch.sh"])
