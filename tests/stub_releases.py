"""The stub releases server of the handoff's pre-release checks (0.10.3, "Where checks run").

It serves a folder in GitHub's release-download layout, ``<root>/v<version>/<name>`` at
``/v<version>/<name>``, and appends each requested path to a log, ``stub-requests.log``.
``AISQUARE_RELEASES_URL`` set to its URL points ``rlx daemon install`` at it.

tests/test_rlxd_install.py uses :func:`serving`. .github/workflows/rlxd-release.yml runs it
as a program over the ``rlxd-archives`` artifact:

    python tests/stub_releases.py ROOT --log stub-requests.log --url-file url.txt

Standard library only, and it binds 127.0.0.1 on a free port.
"""

from __future__ import annotations

import argparse
import contextlib
import functools
import http.server
import threading
from collections.abc import Iterator
from pathlib import Path
from typing import Any


class _Handler(http.server.SimpleHTTPRequestHandler):
    log_path: Path | None = None

    def _record(self) -> None:
        if self.log_path is not None:
            with self.log_path.open("a", encoding="utf-8") as log:
                log.write(self.path + "\n")

    def do_GET(self) -> None:
        self._record()
        super().do_GET()

    def do_HEAD(self) -> None:
        self._record()
        super().do_HEAD()

    def log_message(self, format: str, *args: Any) -> None:
        """Quiet: the log file is the record."""


def _server(root: Path, log: Path | None) -> http.server.ThreadingHTTPServer:
    handler = type("StubReleasesHandler", (_Handler,), {"log_path": log})
    return http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), functools.partial(handler, directory=str(root))
    )


@contextlib.contextmanager
def serving(root: Path, log: Path | None = None) -> Iterator[str]:
    """Serve ``root`` on a thread while the block runs; yields the base URL."""
    server = _server(root, log)
    thread = threading.Thread(target=server.serve_forever, name="stub-releases", daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Serve a folder as a stub releases server.")
    parser.add_argument("root", type=Path)
    parser.add_argument("--log", type=Path, default=Path("stub-requests.log"))
    parser.add_argument("--url-file", type=Path, required=True)
    args = parser.parse_args(argv)
    server = _server(args.root, args.log.resolve())
    args.url_file.write_text(f"http://127.0.0.1:{server.server_address[1]}\n", encoding="utf-8")
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
