"""Deck: a Stream Deck-style controller for Isocast, as a plugin.

A page on 127.0.0.1 with a button per scene, Record, Marker and Split. It
talks to isocastd over its method table (JSON-RPC 2.0, one message per
line, on the Unix socket in $ISOCAST_SOCKET): `get` and `status` for the
scenes and what is on air, `watch` to follow changes, and `switch_scene`,
`marker`, `split`, `start` and `stop` for the buttons. Its grant in
isocast-plugin.toml is exactly those methods; anything else is refused.

Plain Python, standard library only. isocastd starts it, restarts it if it
crashes, and stops it when the plugin is removed or turned off.
"""

import json
import os
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SOCKET = os.environ.get("ISOCAST_SOCKET", "")
PORT = 7650
# The events the page follows. Asking for only these, isocastd doesn't wake
# the deck for the rest (audio levels alone come 20 times a second).
WATCH = {
    "jsonrpc": "2.0",
    "id": 1,
    "method": "watch",
    "params": {"kinds": ["setup_changed", "scene_changed", "started", "stopped"]},
}


def log(message):
    print(f"deck: {message}", flush=True)


class RpcError(Exception):
    pass


class Isocast:
    """Calls to isocastd on one connection, reconnecting when it drops."""

    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.file = None
        self.next_id = 1

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(self.path)
        return sock.makefile("rwb")

    def call(self, method, params=None):
        with self.lock:
            for attempt in (1, 2):
                try:
                    if self.file is None:
                        self.file = self.connect()
                    request = {"jsonrpc": "2.0", "id": self.next_id, "method": method}
                    self.next_id += 1
                    if params is not None:
                        request["params"] = params
                    self.file.write((json.dumps(request) + "\n").encode())
                    self.file.flush()
                    line = self.file.readline()
                    if not line:
                        raise ConnectionError("isocastd closed the connection")
                    response = json.loads(line)
                    if "error" in response:
                        raise RpcError(response["error"]["message"])
                    return response["result"]
                except (OSError, ConnectionError):
                    self.file = None
                    if attempt == 2:
                        raise


class Deck:
    """What the page shows, kept up to date from isocastd's events."""

    def __init__(self, isocast):
        self.isocast = isocast
        self.lock = threading.Lock()
        self.scenes = []
        self.scene = None
        self.recording = False
        self.connected = False
        self.message = ""

    def refresh(self):
        live = self.isocast.call("get")
        status = self.isocast.call("status")
        setup = live.get("setup") or {}
        with self.lock:
            self.scenes = [
                {"id": s["id"], "name": s.get("name") or s["id"]}
                for s in setup.get("scenes", [])
            ]
            self.scene = status.get("scene")
            self.recording = status.get("state") in ("starting", "recording")

    def on_event(self, event):
        kind = event.get("type")
        if kind == "setup_changed":
            self.refresh()
        elif kind == "scene_changed":
            with self.lock:
                self.scene = event.get("scene")
        elif kind == "started":
            with self.lock:
                self.recording = True
        elif kind == "stopped":
            with self.lock:
                self.recording = False

    def follow(self):
        """Watch isocastd's events for as long as it runs."""
        pause = 0.5
        while True:
            # isocastd started this process: if it is gone, so are we.
            if os.getppid() == 1:
                log("isocastd has gone; exiting")
                os._exit(0)
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(SOCKET)
                events = sock.makefile("rwb")
                events.write((json.dumps(WATCH) + "\n").encode())
                events.flush()
                first = json.loads(events.readline() or b"{}")
                if "error" in first:
                    raise RpcError(first["error"]["message"])
                self.refresh()
                with self.lock:
                    self.connected = True
                pause = 0.5
                for line in events:
                    message = json.loads(line)
                    if message.get("method") == "event":
                        self.on_event(message.get("params") or {})
            except (OSError, ValueError, RpcError) as err:
                log(f"lost isocastd ({err}); trying again")
            with self.lock:
                self.connected = False
            time.sleep(pause)
            pause = min(pause * 2, 5)

    def state(self):
        with self.lock:
            return {
                "scenes": self.scenes,
                "scene": self.scene,
                "recording": self.recording,
                "connected": self.connected,
                "message": self.message,
            }

    def press(self, action, scene=None):
        """A button: what it called, or why it failed."""
        try:
            if action == "scene":
                result = self.isocast.call("switch_scene", {"scene": scene})
                with self.lock:
                    self.scene = result.get("scene", scene)
            elif action == "record":
                with self.lock:
                    recording = self.recording
                if recording:
                    self.isocast.call("stop")
                    with self.lock:
                        self.recording = False
                else:
                    self.isocast.call("start", {})
                    with self.lock:
                        self.recording = True
            elif action == "marker":
                self.isocast.call("marker", {"label": "deck " + time.strftime("%H:%M:%S")})
            elif action == "split":
                self.isocast.call("split")
            else:
                raise RpcError(f"no button {action!r}")
            message = ""
        except (OSError, ConnectionError, RpcError) as err:
            message = str(err)
        with self.lock:
            self.message = message
        return {"ok": not message, "error": message or None}


PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Deck</title>
<style>
:root { color-scheme: dark; --bg: #151515; --pane: #1e1e1e; --line: #2c2c2c;
  --text: #e8e8e8; --dim: #8a8a8a; --air: #3a7d44; --rec: #b3261e; }
body { margin: 0; background: var(--bg); color: var(--text);
  font: 14px -apple-system, system-ui, sans-serif; }
header { padding: 12px 16px; border-bottom: 1px solid var(--line); color: var(--dim); }
main { display: grid; grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
  gap: 1px; background: var(--line); border-bottom: 1px solid var(--line); }
button { aspect-ratio: 1; border: 0; border-radius: 0; background: var(--pane);
  color: var(--text); font: inherit; font-size: 15px; cursor: pointer; padding: 8px; }
button:active { filter: brightness(1.3); }
button.air { background: var(--air); }
button.rec { background: var(--rec); }
#message { padding: 8px 16px; color: #e5a50a; min-height: 1.2em; }
</style></head>
<body><header id="status">Connecting…</header><main id="buttons"></main>
<div id="message"></div>
<script>
async function press(action, scene) {
  const response = await fetch("/press", {method: "POST",
    headers: {"Content-Type": "application/json", "X-Deck": "1"},
    body: JSON.stringify({action, scene})});
  render(await (await fetch("/state")).json());
}
function button(label, cls, onclick) {
  const b = document.createElement("button");
  b.textContent = label; if (cls) b.className = cls; b.onclick = onclick; return b;
}
function render(s) {
  document.getElementById("status").textContent = s.connected
    ? (s.recording ? "Recording" : "Not recording") + (s.scene ? " · on air: " + s.scene : "")
    : "Waiting for isocastd…";
  const main = document.getElementById("buttons");
  main.replaceChildren(
    ...s.scenes.map(sc => button(sc.name, sc.id === s.scene ? "air" : "", () => press("scene", sc.id))),
    button(s.recording ? "Stop" : "Record", s.recording ? "rec" : "", () => press("record")),
    button("Marker", "", () => press("marker")),
    button("Split", "", () => press("split")));
  document.getElementById("message").textContent = s.message || "";
}
async function poll() {
  try { render(await (await fetch("/state")).json()); } catch (e) {}
  setTimeout(poll, 400);
}
poll();
</script></body></html>
"""


def serve(deck):
    allowed_hosts = set()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, code, body, kind="application/json"):
            data = body.encode() if isinstance(body, str) else body
            self.send_response(code)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(data)

        def local(self):
            # Pages from other sites can't reach it: only this address
            # (against DNS rebinding), and presses need a header that a
            # cross-site request can't send without the browser asking.
            return self.headers.get("Host", "") in allowed_hosts

        def do_GET(self):
            if not self.local():
                return self.reply(403, '{"error":"forbidden"}')
            if self.path == "/":
                self.reply(200, PAGE, "text/html; charset=utf-8")
            elif self.path == "/state":
                self.reply(200, json.dumps(deck.state()))
            else:
                self.reply(404, '{"error":"not found"}')

        def do_POST(self):
            if not self.local() or self.headers.get("X-Deck") != "1":
                return self.reply(403, '{"error":"forbidden"}')
            if self.path != "/press":
                return self.reply(404, '{"error":"not found"}')
            length = int(self.headers.get("Content-Length") or 0)
            try:
                body = json.loads(self.rfile.read(length) or b"{}")
            except ValueError:
                return self.reply(400, '{"error":"not JSON"}')
            result = deck.press(body.get("action"), body.get("scene"))
            self.reply(200 if result["ok"] else 409, json.dumps(result))

    try:
        server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    except OSError:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    allowed_hosts.update({f"127.0.0.1:{port}", f"localhost:{port}"})
    log(f"open http://127.0.0.1:{port}/")
    server.serve_forever()


def main():
    if not SOCKET:
        sys.exit("deck: ISOCAST_SOCKET isn't set; isocastd runs this plugin")
    deck = Deck(Isocast(SOCKET))
    threading.Thread(target=deck.follow, daemon=True).start()
    serve(deck)


if __name__ == "__main__":
    main()
