#!/usr/bin/env python3
"""In-sandbox model shim.

Runs *inside* the guest network namespace, where nothing but loopback exists.
It listens on the address the guest's Pi models.json points at and forwards each
TCP connection to the broker's allowlisting Unix-socket proxy, then execs the
guest command.

Usage:
    python3 model-shim.py --listen 127.0.0.1:11434 \
        --upstream-unix /run/guest-io/model-proxy.sock -- <command> [args...]

Exit status is the guest command's status.
"""

from __future__ import annotations

import argparse
import os
import socket
import socketserver
import sys
import threading


class _Forward(socketserver.BaseRequestHandler):
    upstream_path = ""

    def handle(self) -> None:
        try:
            up = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            up.settimeout(120)
            up.connect(self.upstream_path)
        except OSError as exc:
            print(f"model-shim: cannot reach broker proxy: {exc}", file=sys.stderr, flush=True)
            return
        self.request.settimeout(120)

        def pump(src: socket.socket, dst: socket.socket) -> None:
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

        t = threading.Thread(target=pump, args=(self.request, up), daemon=True)
        t.start()
        pump(up, self.request)
        t.join(timeout=5)
        up.close()


class _Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if "--" not in argv:
        print("model-shim: missing '-- <command>'", file=sys.stderr)
        return 2
    split = argv.index("--")
    opts, command = argv[:split], argv[split + 1:]
    parser = argparse.ArgumentParser(prog="model-shim")
    parser.add_argument("--listen", default="127.0.0.1:11434")
    parser.add_argument("--upstream-unix", required=True)
    args = parser.parse_args(opts)
    if not command:
        print("model-shim: empty command", file=sys.stderr)
        return 2

    host, _, port_text = args.listen.rpartition(":")
    handler = type("Handler", (_Forward,), {"upstream_path": args.upstream_unix})
    server = _Server((host or "127.0.0.1", int(port_text)), handler)
    threading.Thread(target=server.serve_forever, name="shim-listener", daemon=True).start()

    try:
        # Preserve the sanitized guest environment produced by bwrap/Podman
        # (HOME, PI_CODING_AGENT_DIR, PI_CODING_AGENT_SESSION_DIR, TMPDIR, ...).
        # Only the shim's own arguments are added by this process.
        pid = os.posix_spawnp(command[0], command, os.environ.copy())
    except OSError as exc:
        print(f"model-shim: cannot exec {command[0]!r}: {exc}", file=sys.stderr)
        server.shutdown()
        return 127
    _, status = os.waitpid(pid, 0)
    server.shutdown()
    if os.WIFEXITED(status):
        return os.WEXITSTATUS(status)
    if os.WIFSIGNALED(status):
        return 128 + os.WTERMSIG(status)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
