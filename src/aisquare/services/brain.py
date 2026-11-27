"""The resident Python brain: ``rlx brain --stdio``, which rlxd supervises (handoff 0.10.3,
1.6).

rlxd starts one brain per home and keeps it running: it starts it again, with backoff, when
it dies, and kills its process tree when rlxd stops. What rlxd asks Python, it asks the
brain, so no Python process starts for each question (1.5's hooks first; later the
connector map and the runner's prompts and board calls).

The protocol
------------

JSON lines over the brain's stdin (rlxd's frames) and stdout (the brain's). Every frame is
one JSON object on one line, ASCII only, with a ``type``. The shapes are rlxd-proto's
(``rlxd/schemas/v1/brain-input.json`` and ``brain-output.json``):

1. rlxd says ``{"type": "hello", "protocol": 1, "rlxd_version": "…"}`` first.
2. The brain answers ``{"type": "hello", "protocol": 1, "pid": …, "rlx_version": "…"}``,
   or, for a ``protocol`` it does not speak, ``{"type": "refused", "code":
   "incompatible_protocol", "message": "…", "supported": [1]}`` and exits. rlxd refuses
   a brain whose ``protocol`` it does not speak the same way. So an rlx and an rlxd of
   different releases fail loudly, with a named error in rlxd's ``status``, and never
   misread each other's frames.
3. Then requests, one at a time: ``{"type": "request", "id": 7, "method": "ping",
   "params": {}}``. The brain answers each with ``{"type": "accepted", "id": 7}`` as soon
   as it has read it, before it runs anything, then with ``{"type": "reply", "id": 7,
   "ok": true, "result": {…}}`` or ``{"type": "reply", "id": 7, "ok": false, "error":
   {"code": "…", "message": "…"}}``. ``accepted`` is the line a caller's fallback turns
   on: a request the brain never accepted was never run, and may be run elsewhere; one it
   accepted may have been, and must not be run again.

rlxd keeps one request in flight, so the brain runs them strictly one after another. When
its stdin closes (rlxd stopped or died), the brain exits.

The channel
-----------

The brain moves the protocol off fds 0 and 1 before it reads anything: it reads and
writes private duplicates of them, points fd 0 at the null device, and points fd 1 at
fd 2, its log. So nothing the brain runs, and no process it starts, can read a frame
meant for it or write into its replies: a stray print lands in rlxd's log of the brain.
What the process writes to stdout before that (as it imports and starts) reaches rlxd
before the hello: rlxd logs a line that is not JSON and passes over it, and takes any
JSON line that is not a hello of its protocol for a brain of another release.
"""

from __future__ import annotations

import json
import os
import sys
from collections.abc import Callable, Mapping
from typing import IO, Any

#: The brain protocols this brain speaks. rlxd's own list is rlxd-proto's
#: ``BRAIN_PROTOCOLS``; they must share one, or rlxd reports ``brain_incompatible``.
PROTOCOLS: tuple[int, ...] = (1,)

#: The longest frame the brain reads: 16 MiB. A hook's payload is at most rlxd's own
#: frame limit (1 MiB), base64 and all.
MAX_FRAME = 16 << 20

Result = dict[str, Any]
Method = Callable[[Mapping[str, Any]], Result]


class BrainError(Exception):
    """A request the brain refused or failed: its ``code`` and ``message`` are the reply's
    ``error``."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code
        self.message = message


class ProtocolError(Exception):
    """A frame from rlxd that is not one this protocol has: the brain says why on its log
    and exits, rather than guess."""


def _ping(params: Mapping[str, Any]) -> Result:
    """``ping``: the brain is here, and answers. ``{"pid": …}``."""
    del params
    return {"pid": os.getpid()}


#: The methods, by name.
METHODS: dict[str, Method] = {"ping": _ping}


def _rlx_version() -> str:
    from aisquare.core.version import __version__

    return __version__


class Channel:
    """The protocol's two ends: frames read from ``reader``, written to ``writer``."""

    def __init__(self, reader: IO[bytes], writer: IO[bytes]) -> None:
        self._reader = reader
        self._writer = writer

    def read(self) -> dict[str, Any] | None:
        """The next frame, or ``None`` at the end of the input. Raises
        :class:`ProtocolError` for a line that is not one JSON object, or too long."""
        line = self._reader.readline(MAX_FRAME + 1)
        if not line:
            return None
        if len(line) > MAX_FRAME:
            raise ProtocolError(f"a frame is longer than {MAX_FRAME} bytes")
        try:
            frame = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise ProtocolError(f"a frame is not JSON: {exc}") from exc
        if not isinstance(frame, dict):
            raise ProtocolError("a frame is not a JSON object")
        return frame

    def write(self, frame: Mapping[str, Any]) -> None:
        """Writes ``frame`` as one line, at once."""
        line = json.dumps(frame, ensure_ascii=True, separators=(",", ":")) + "\n"
        self._writer.write(line.encode("ascii"))
        self._writer.flush()


def _say(message: str) -> None:
    """One line on the brain's log (its stderr, which rlxd keeps)."""
    sys.stderr.write(f"brain: {message}\n")
    sys.stderr.flush()


def _protocol_of(frame: Mapping[str, Any]) -> int | None:
    protocol = frame.get("protocol")
    if isinstance(protocol, bool) or not isinstance(protocol, int):
        return None
    return protocol


def greet(channel: Channel, frame: Mapping[str, Any]) -> bool:
    """Answers rlxd's hello, ``frame``: whether rlxd and this brain share a protocol.
    Refuses, and says why, when they do not."""
    if frame.get("type") != "hello":
        raise ProtocolError(f"the first frame must be hello, not {frame.get('type')!r}")
    protocol = _protocol_of(frame)
    if protocol not in PROTOCOLS:
        message = (
            f"rlxd speaks brain protocol {frame.get('protocol')!r}, and this rlx "
            f"{_rlx_version()} speaks {', '.join(map(str, PROTOCOLS))}: upgrade the older one"
        )
        channel.write(
            {
                "type": "refused",
                "code": "incompatible_protocol",
                "message": message,
                "supported": list(PROTOCOLS),
            }
        )
        _say(message)
        return False
    channel.write(
        {"type": "hello", "protocol": protocol, "pid": os.getpid(), "rlx_version": _rlx_version()}
    )
    return True


def answer(method: str, params: Mapping[str, Any], methods: Mapping[str, Method]) -> Result:
    """One request's reply frame, less its ``type`` and ``id``: never raises."""
    handler = methods.get(method)
    if handler is None:
        return {
            "ok": False,
            "error": {"code": "unknown_method", "message": f"the brain has no method {method!r}"},
        }
    try:
        return {"ok": True, "result": handler(params)}
    except BrainError as exc:
        return {"ok": False, "error": {"code": exc.code, "message": exc.message}}
    except Exception as exc:  # a failed request never ends the brain
        _say(f"{method} failed: {type(exc).__name__}: {exc}")
        return {
            "ok": False,
            "error": {"code": "brain_failed", "message": f"{type(exc).__name__}: {exc}"},
        }


def serve(channel: Channel, methods: Mapping[str, Method] | None = None) -> int:
    """Greets rlxd, then answers its requests until its frames end: the exit status."""
    methods = METHODS if methods is None else methods
    try:
        hello = channel.read()
        if hello is None:
            return 0  # rlxd went away before it said anything
        if not greet(channel, hello):
            return 3
        while True:
            frame = channel.read()
            if frame is None:
                return 0
            if frame.get("type") != "request":
                raise ProtocolError(f"expected a request, not {frame.get('type')!r}")
            request_id = frame.get("id")
            method = frame.get("method")
            params = frame.get("params", {})
            if (
                isinstance(request_id, bool)
                or not isinstance(request_id, int)
                or not isinstance(method, str)
                or not isinstance(params, dict)
            ):
                raise ProtocolError("a request needs an integer id, a method and object params")
            channel.write({"type": "accepted", "id": request_id})
            channel.write({"type": "reply", "id": request_id, **answer(method, params, methods)})
    except ProtocolError as exc:
        _say(f"{exc}; exiting")
        return 2
    except (BrokenPipeError, ConnectionResetError):
        return 0  # rlxd is gone


def private_channel() -> Channel:
    """The protocol on private duplicates of fds 0 and 1, which nothing else uses: fd 0 is
    pointed at the null device and fd 1 at fd 2 (the brain's log) from here on."""
    reader = os.fdopen(os.dup(0), "rb")
    writer = os.fdopen(os.dup(1), "wb")
    sys.stdout.flush()
    null = os.open(os.devnull, os.O_RDWR)
    try:
        os.dup2(null, 0)
    finally:
        os.close(null)
    os.dup2(2, 1)
    return Channel(reader, writer)


def serve_stdio() -> int:
    """``rlx brain --stdio``: the protocol on this process's stdin and stdout."""
    return serve(private_channel())
