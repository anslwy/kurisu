#!/usr/bin/env python3
"""In-container relay for the guest runner (stdlib only).

The guest container runs with ``--network=none``: only its own loopback exists.
This relay listens on fixed container-loopback addresses and forwards each TCP
connection to a fixed Unix socket in the read-only ``/run/kurisu-io`` mount,
then execs the runner command (``omnigent host ...``).

    instance-relay.py --forward 127.0.0.1:<port>=/run/kurisu-io/omnigent.sock \
                      --forward 127.0.0.1:11434=/run/kurisu-io/model-proxy.sock \
                      -- omnigent host --server http://127.0.0.1:<port> ...

The host side of ``omnigent.sock`` connects only to this guest's dedicated
instance; ``model-proxy.sock`` is the broker's allowlisting model proxy.
"""

from __future__ import annotations

import argparse
import socket
import socketserver
import sys
import threading


def _pump(src: socket.socket, dst: socket.socket) -> None:
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    finally:
        try:
            dst.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def _make_handler(unix_path: str):
    class Handler(socketserver.BaseRequestHandler):
        def handle(self) -> None:
            up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                up.connect(unix_path)
            except OSError as exc:
                print(f"instance-relay: {unix_path} unreachable: {exc}", file=sys.stderr, flush=True)
                up.close()
                return
            t = threading.Thread(target=_pump, args=(up, self.request), daemon=True)
            t.start()
            _pump(self.request, up)
            t.join()
            up.close()

    return Handler


class _Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--forward", action="append", required=True,
                        help="127.0.0.1:PORT=/run/kurisu-io/NAME.sock")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("missing runner command after --")
    for spec in args.forward:
        addr, _, path = spec.partition("=")
        host, _, port = addr.rpartition(":")
        if host != "127.0.0.1" or not path.startswith("/run/kurisu-io/"):
            parser.error(f"refusing forward {spec!r}: loopback -> /run/kurisu-io only")
        server = _Server((host, int(port)), _make_handler(path))
        threading.Thread(target=server.serve_forever, daemon=True).start()
    # Keep the relay threads alive in this process; run the runner as a child.
    import subprocess

    return subprocess.call(command)


if __name__ == "__main__":
    raise SystemExit(main())
