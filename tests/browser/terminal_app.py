# Runs the terminal app on the invented data for a while, then quits it. Exits 1 if it misbehaved.
import fcntl, os, pty, select, struct, subprocess, sys, termios, time

binp, home, secs = sys.argv[1], sys.argv[2], float(sys.argv[3])
m, s = pty.openpty()
fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
p = subprocess.Popen([binp], stdin=s, stdout=s, stderr=s, env={"PATH": os.environ["PATH"], "HOME": home, "TERM": "xterm-256color", "GIT_OPTIONAL_LOCKS": "0"}, close_fds=True, start_new_session=True)
os.close(s)
end = time.time() + secs
buf = b""
while time.time() < end and p.poll() is None:
    if select.select([m], [], [], 0.2)[0]:
        try:
            buf += os.read(m, 65536)
        except OSError:
            break
os.write(m, b"q")
end = time.time() + 10
while time.time() < end and p.poll() is None:
    if select.select([m], [], [], 0.1)[0]:
        try:
            os.read(m, 65536)
        except OSError:
            pass
code = p.poll()
if code is None:
    p.kill()
listed = b"conversations" in buf
refresh_failed = b"Refresh failed" in buf
print("exit", code, "showed list:", listed, "failed refresh:", refresh_failed)
sys.exit(0 if code == 0 and listed and not refresh_failed else 1)
