"""Real child/socket regressions for managed SDK runtime ownership."""

from __future__ import annotations

import os
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import conftest
from conftest import SqrzlRuntime, _reserve_port


class ForeignHealth(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        pass

    def handle(self):
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            pass


class RuntimeChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="sqrzl-runtime-check-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def runtime(self, code, port, ui_port=None):
        binary = self.root / "controlled-child"
        binary.write_text(f"#!{sys.executable}\n{code}\n")
        binary.chmod(0o755)
        runtime = SqrzlRuntime(
            binary,
            dict(os.environ),
            self.root,
            f"http://127.0.0.1:{port}",
            ui_url=f"http://127.0.0.1:{ui_port}" if ui_port is not None else None,
        )

        def cleanup():
            try:
                runtime.stop(kill=True)
            except RuntimeError:
                pass

        self.addCleanup(cleanup)
        return runtime

    def owned_server(self, port, exit_later=False, ui_port=None):
        timer = "threading.Timer(1, lambda: os._exit(1)).start()" if exit_later else ""
        return self.runtime(
            "import os, threading\n"
            "from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer\n"
            "class Health(BaseHTTPRequestHandler):\n"
            " protocol_version = 'HTTP/1.1'\n"
            " def do_GET(self):\n"
            "  self.send_response(200); self.send_header('Content-Length', '2'); self.end_headers(); self.wfile.write(b'ok')\n"
            " def log_message(self, *args): pass\n"
            f"{timer}\n"
            + (
                f"threading.Thread(target=ThreadingHTTPServer(('127.0.0.1', {ui_port}), Health).serve_forever, daemon=True).start()\n"
                if ui_port
                else ""
            )
            + f"ThreadingHTTPServer(('127.0.0.1', {port}), Health).serve_forever()",
            port,
            ui_port,
        )

    def test_foreign_health_does_not_qualify_owned_child(self):
        foreign = ThreadingHTTPServer(("127.0.0.1", 0), ForeignHealth)
        thread = threading.Thread(target=foreign.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(foreign.server_close)
        self.addCleanup(foreign.shutdown)
        runtime = self.runtime(
            "import time; time.sleep(0.3); raise SystemExit(1)", foreign.server_port
        )
        with self.assertRaisesRegex(RuntimeError, "startup failed"):
            runtime.start()
        self.assertIsNone(runtime.process)
        self.assertIsNone(runtime._log_file)
        with self.assertRaises(ProcessLookupError):
            os.kill(runtime.events[-1]["pid"], 0)

    def test_foreign_ui_health_does_not_qualify_owned_api_child(self):
        foreign = ThreadingHTTPServer(("127.0.0.1", 0), ForeignHealth)
        threading.Thread(target=foreign.serve_forever, daemon=True).start()
        self.addCleanup(foreign.server_close)
        self.addCleanup(foreign.shutdown)
        runtime = self.owned_server(_reserve_port(), exit_later=True)
        runtime.health_addresses["ui"] = f"http://127.0.0.1:{foreign.server_port}"
        with self.assertRaisesRegex(RuntimeError, "startup failed"):
            runtime.start()
        self.assertIsNone(runtime.process)
        self.assertIsNone(runtime._log_file)

    def test_health_connection_belongs_to_child_across_restart(self):
        runtime = self.owned_server(_reserve_port(), ui_port=_reserve_port())
        old = runtime.start()
        new = runtime.restart()
        self.assertNotEqual(old, new)
        self.assertEqual(runtime.stop(), new)
        self.assertEqual(runtime.stop(), None)
        self.assertEqual(
            [event["kind"] for event in runtime.events],
            ["start", "normal-stop", "start", "normal-stop"],
        )
        self.assertEqual(set(runtime.events[0]["health_addresses"]), {"api", "ui"})
        self.assertEqual(
            runtime.events[0]["health_ownership"], "accepted-connection-child-pid"
        )

    def test_linux_proc_requires_owned_inode_and_both_health_endpoints(self):
        root = self.root / "proc-child"
        (root / "fd").mkdir(parents=True)
        (root / "net").mkdir()
        (root / "fd/7").symlink_to("socket:[111]")
        process = SimpleNamespace(pid=42, poll=lambda: None)
        tcp = root / "net/tcp"
        valid = "0: 0100007F:2328 0100007F:C350 01 0:0 0:0 0 501 0 111"
        for line, expected in [
            (valid, True),
            (valid.replace("111", "222"), False),
            (valid.replace("C350", "C351"), False),
            (valid.replace("2328", "2329"), False),
            (valid.replace(" 01 ", " 0A "), False),
        ]:
            with self.subTest(record=line):
                tcp.write_text("header\n" + line + "\n")
                with (
                    patch.object(conftest.sys, "platform", "linux"),
                    patch.object(conftest, "Path", return_value=root),
                ):
                    self.assertEqual(
                        conftest._owns_health_connection(process, 9000, 50000), expected
                    )

    def test_unexpected_child_exit_cannot_be_recorded_as_normal_stop(self):
        runtime = self.owned_server(_reserve_port(), exit_later=True)
        runtime.start()
        self.assertEqual(runtime.process.wait(timeout=5), 1)
        with self.assertRaisesRegex(RuntimeError, "unexpected"):
            runtime.stop()
        self.assertEqual(runtime.events[-1]["kind"], "unexpected-stop")


if __name__ == "__main__":
    unittest.main()
