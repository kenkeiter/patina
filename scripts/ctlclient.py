#!/usr/bin/env python3
"""Tiny scripted client for the patina control channel (tests only).

    python3 -I scripts/ctlclient.py SOCKET [--connect-timeout S] [--linger S] ACTION...

Connects to the Unix socket (retrying until --connect-timeout, default 10 s:
QEMU creates it at start-up), then runs the actions in order while printing
every received line as "<< line" (and every sent one as ">> line"):

    send:JSON          send one NDJSON line
    wait:TEXT          wait until a received line contains TEXT (15 s)
    wait@SECS:TEXT     the same with a timeout of SECS
    sleep:SECS         sleep
    reconnect          close the connection and connect again
    log:TEXT           wait (60 s) until the file given with --log (the
                       guest serial log) contains TEXT; events keep printing

After the actions it keeps printing received lines until the guest closes
the connection or --linger seconds (default 5) pass without data. Exit
status: 0 if every wait matched, 1 otherwise (2 if it never connected).
"""
import os
import socket
import sys
import time


def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__, file=sys.stderr)
        return 2
    path = args.pop(0)
    connect_timeout, linger = 10.0, 5.0
    logfile = None
    actions = []
    while args:
        a = args.pop(0)
        if a == "--connect-timeout":
            connect_timeout = float(args.pop(0))
        elif a == "--log":
            logfile = args.pop(0)
        elif a == "--linger":
            linger = float(args.pop(0))
        else:
            actions.append(a)

    def connect(timeout):
        t0 = time.time()
        while time.time() - t0 < timeout:
            c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                c.connect(path)
                print("ctlclient: connected after %.2fs" % (time.time() - t0), flush=True)
                return c
            except OSError:
                c.close()
                time.sleep(0.05)
        return None

    s = connect(connect_timeout)
    if s is None:
        print("ctlclient: could not connect to %s" % path, file=sys.stderr)
        return 2

    buf = b""
    seen = []
    closed = False

    def pump(timeout):
        nonlocal buf, closed
        if closed:
            return False
        s.settimeout(max(timeout, 0.001))
        try:
            d = s.recv(65536)
        except socket.timeout:
            return False
        except OSError:
            closed = True
            return False
        if not d:
            closed = True
            return False
        buf += d
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            text = line.decode("utf-8", "replace").rstrip("\r")
            seen.append(text)
            print("<< " + text, flush=True)
        return True

    ok = True
    last_send = time.time()
    for a in actions:
        kind, _, rest = a.partition(":")
        if kind == "send":
            print(">> " + rest, flush=True)
            try:
                s.sendall(rest.encode() + b"\n")
                last_send = time.time()
            except OSError as e:
                print("ctlclient: send failed: %s" % e, flush=True)
                ok = False
        elif kind == "log":
            end = time.time() + 60
            found = False
            while time.time() < end and not found:
                try:
                    with open(logfile, "rb") as f:
                        found = rest.encode() in f.read()
                except OSError:
                    pass
                if not found and not pump(0.1):
                    time.sleep(0.05)
            print("ctlclient: serial %s %r" % ("has" if found else "TIMEOUT waiting for", rest), flush=True)
            ok = ok and found
        elif kind == "reconnect":
            s.close()
            time.sleep(0.5)
            s = connect(5)
            buf, closed = b"", s is None
            print("ctlclient: reconnected" if s else "ctlclient: reconnect failed", flush=True)
            if s is None:
                ok = False
                break
        elif kind == "sleep":
            end = time.time() + float(rest)
            while time.time() < end:
                pump(end - time.time())
        elif kind == "wait" or kind.startswith("wait@"):
            text = rest
            secs = kind[5:] or "15"
            end = time.time() + float(secs)
            start = len(seen)
            found = False
            # Lines that arrived before this wait count too.
            if any(text in l for l in seen):
                found = True
            while not found and time.time() < end and not closed:
                pump(end - time.time())
                found = any(text in l for l in seen[start:])
            if not found:
                print("ctlclient: TIMEOUT waiting for %r" % text, flush=True)
                ok = False
            else:
                print("ctlclient: matched %r %.2fs after the last send" % (text, time.time() - last_send), flush=True)
        else:
            print("ctlclient: unknown action %r" % a, file=sys.stderr)
            return 2
    while not closed and pump(linger):
        pass
    if s:
        s.close()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
