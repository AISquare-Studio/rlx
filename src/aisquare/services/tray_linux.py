"""The tray's Linux half (#34): a StatusNotifierItem and its dbusmenu on the session bus.

Through jeepney, pure Python, in the ``tray`` extra: no PyGObject and no system library, which
a uv tool venv could not see. KDE shows the item itself; GNOME shows it with the AppIndicator
extension, and without it there is no ``org.kde.StatusNotifierWatcher`` on the bus: the tray
exits with ``EXIT_NO_HOST`` and the service starts it no more (``service_tray``).

:class:`Item` answers the host's calls, one message in and the messages out, and touches no
bus, so the suite drives it with jeepney's own messages (``tests/test_tray.py``). The host's
activation (a click, or a double-click, as the desktop has it) opens rlx; the context menu is
the dbusmenu, rebuilt from the service's status when the host says it is about to show it.
``template`` is macOS's: here it is the backslash in rlx's blue, which a dark panel shows.

Imported by ``services.tray.backend_for`` alone, in ``rlx service tray``.
"""

from __future__ import annotations

import ctypes
import logging
import os
import signal
from collections.abc import Sequence
from typing import Any

from jeepney import (
    DBusAddress,
    HeaderFields,
    Message,
    MessageType,
    new_error,
    new_method_call,
    new_method_return,
    new_signal,
)
from jeepney.bus_messages import MatchRule, message_bus
from jeepney.io.blocking import DBusConnection, open_dbus_connection

from aisquare.core import glyph
from aisquare.core.version import PRODUCT_NAME
from aisquare.services.service_tray import EXIT_NO_HOST
from aisquare.services.tray import OPEN, MenuItem, Tray

_log = logging.getLogger(__name__)

ITEM_PATH = "/StatusNotifierItem"
MENU_PATH = "/MenuBar"
ITEM_INTERFACE = "org.kde.StatusNotifierItem"
MENU_INTERFACE = "com.canonical.dbusmenu"
PROPERTIES = "org.freedesktop.DBus.Properties"
INTROSPECTABLE = "org.freedesktop.DBus.Introspectable"
WATCHER = DBusAddress(
    "/StatusNotifierWatcher", "org.kde.StatusNotifierWatcher", "org.kde.StatusNotifierWatcher"
)
UNKNOWN_METHOD = "org.freedesktop.DBus.Error.UnknownMethod"
ICON_SIZES = (16, 22, 24, 32, 48)
"""The pixmaps the item offers: the host takes the one nearest its panel's size."""
PR_SET_PDEATHSIG = 1

Variant = tuple[str, Any]
Layout = tuple[int, dict[str, Variant], list[Variant]]


def pixmaps(icon: str) -> list[tuple[int, int, bytes]]:
    """``IconPixmap``: the glyph at each of :data:`ICON_SIZES`, ARGB32."""
    choice: glyph.Icon = "mark" if glyph.icon_named(icon) == "mark" else "backslash"
    images = [glyph.icon(choice, size) for size in ICON_SIZES]
    return [(image.width, image.height, image.argb()) for image in images]


class Item:
    """The StatusNotifierItem at :data:`ITEM_PATH` and its menu at :data:`MENU_PATH`."""

    def __init__(self, tray: Tray, *, icon: str = "backslash") -> None:
        self.tray = tray
        self.revision = 1
        self._icons = pixmaps(icon)
        self._actions: dict[int, str] = {}
        self._layout: Layout = (0, {}, [])
        self._rows: list[MenuItem] = []
        self.rebuild()

    # --- the menu --------------------------------------------------------------------------

    def rebuild(self) -> bool:
        """The menu from the service's status as it is now; whether it changed."""
        rows = self.tray.items()
        if rows == self._rows:
            return False
        self._rows = rows
        self._actions = {}
        counter = iter(range(1, 10_000))
        children = [self._entry(row, counter) for row in rows]
        self._layout = (0, {"children-display": ("s", "submenu")}, children)
        self.revision += 1
        return True

    def _entry(self, row: MenuItem, counter: Any) -> Variant:
        number = next(counter)
        props: dict[str, Variant] = {}
        if row.separator:
            props["type"] = ("s", "separator")
        else:
            props["label"] = ("s", row.label.replace("_", "__"))  # "_" marks an access key
            props["enabled"] = ("b", row.action is not None or bool(row.children))
        if row.action is not None:
            self._actions[number] = row.action
        children = [self._entry(child, counter) for child in row.children]
        if children:
            props["children-display"] = ("s", "submenu")
        return ("(ia{sv}av)", (number, props, children))

    def _find(self, number: int, layout: Layout | None = None) -> Layout | None:
        layout = self._layout if layout is None else layout
        if layout[0] == number:
            return layout
        for _, child in layout[2]:
            found = self._find(number, child)
            if found is not None:
                return found
        return None

    # --- the properties --------------------------------------------------------------------

    def item_properties(self) -> dict[str, Variant]:
        lines = [row.label for row in self._rows if row.action is None and not row.separator]
        return {
            "Category": ("s", "ApplicationStatus"),
            "Id": ("s", PRODUCT_NAME),
            "Title": ("s", PRODUCT_NAME),
            "Status": ("s", "Active"),
            "WindowId": ("i", 0),
            "IconName": ("s", ""),
            "IconPixmap": ("a(iiay)", self._icons),
            "OverlayIconName": ("s", ""),
            "AttentionIconName": ("s", ""),
            "ToolTip": ("(sa(iiay)ss)", ("", [], PRODUCT_NAME, " · ".join(lines))),
            "ItemIsMenu": ("b", False),
            "Menu": ("o", MENU_PATH),
        }

    @staticmethod
    def menu_properties() -> dict[str, Variant]:
        return {
            "Version": ("u", 3),
            "TextDirection": ("s", "ltr"),
            "Status": ("s", "normal"),
            "IconThemePath": ("as", []),
        }

    # --- the calls -------------------------------------------------------------------------

    def handle(self, msg: Message) -> list[Message]:
        """The replies (and signals) for one message from the bus: none for anything that is
        not a method call to one of ours."""
        if msg.header.message_type is not MessageType.method_call:
            return []
        fields = msg.header.fields
        path = fields.get(HeaderFields.path)
        interface = fields.get(HeaderFields.interface)
        member = fields.get(HeaderFields.member)
        if path not in (ITEM_PATH, MENU_PATH):
            return [new_error(msg, UNKNOWN_METHOD, "s", (f"no object at {path}",))]
        try:
            return self._call(msg, path, interface, member)
        except Exception as exc:  # one bad call is an error back, never the end of the tray
            _log.warning("the tray could not answer %s.%s", interface, member, exc_info=True)
            return [new_error(msg, "org.freedesktop.DBus.Error.Failed", "s", (str(exc),))]

    def _call(
        self, msg: Message, path: str, interface: str | None, member: str | None
    ) -> list[Message]:
        if interface == INTROSPECTABLE and member == "Introspect":
            return [new_method_return(msg, "s", (_introspection(path),))]
        if interface == PROPERTIES:
            props = self.item_properties() if path == ITEM_PATH else self.menu_properties()
            if member == "GetAll":
                return [new_method_return(msg, "a{sv}", (props,))]
            if member == "Get":
                _, name = msg.body
                if name in props:
                    return [new_method_return(msg, "v", (props[name],))]
                return [new_error(msg, "org.freedesktop.DBus.Error.UnknownProperty", "s", (name,))]
        if path == ITEM_PATH and interface in (ITEM_INTERFACE, None):
            if member in ("Activate", "SecondaryActivate"):
                self.tray.act(OPEN)
                return [new_method_return(msg)]
            if member in ("ContextMenu", "Scroll"):
                return [new_method_return(msg)]
        if path == MENU_PATH and interface in (MENU_INTERFACE, None):
            return self._menu_call(msg, member)
        return [new_error(msg, UNKNOWN_METHOD, "s", (f"{interface}.{member}",))]

    def _menu_call(self, msg: Message, member: str | None) -> list[Message]:
        if member == "GetLayout":
            parent, _depth, _names = msg.body
            layout = self._find(parent) or self._layout
            return [new_method_return(msg, "u(ia{sv}av)", (self.revision, layout))]
        if member == "GetGroupProperties":
            numbers, _names = msg.body
            found = [(n, entry[1]) for n in numbers if (entry := self._find(n)) is not None]
            return [new_method_return(msg, "a(ia{sv})", (found,))]
        if member == "GetProperty":
            number, name = msg.body
            entry = self._find(number)
            if entry is None or name not in entry[1]:
                return [new_error(msg, "org.freedesktop.DBus.Error.InvalidArgs", "s", (name,))]
            return [new_method_return(msg, "v", (entry[1][name],))]
        if member == "Event":
            number, event, _data, _when = msg.body
            self._clicked(number, event)
            return [new_method_return(msg)]
        if member == "EventGroup":
            (events,) = msg.body
            missing = [number for number, *_ in events if self._find(number) is None]
            for number, event, *_ in events:
                self._clicked(number, event)
            return [new_method_return(msg, "ai", (missing,))]
        if member == "AboutToShow":
            changed = self.rebuild()
            out = [new_method_return(msg, "b", (changed,))]
            return out + ([self.layout_updated()] if changed else [])
        if member == "AboutToShowGroup":
            changed = self.rebuild()
            updates = [0] if changed else []
            out = [new_method_return(msg, "(aiai)", ((updates, []),))]
            return out + ([self.layout_updated()] if changed else [])
        return [new_error(msg, UNKNOWN_METHOD, "s", (f"{MENU_INTERFACE}.{member}",))]

    def _clicked(self, number: int, event: str) -> None:
        action = self._actions.get(number)
        if event == "clicked" and action is not None:
            self.tray.act(action)

    def layout_updated(self) -> Message:
        return new_signal(
            DBusAddress(MENU_PATH, interface=MENU_INTERFACE),
            "LayoutUpdated",
            "ui",
            (self.revision, 0),
        )


def _introspection(path: str) -> str:
    interface = ITEM_INTERFACE if path == ITEM_PATH else MENU_INTERFACE
    return (
        '<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" '
        '"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">'
        f'<node><interface name="{interface}"/><interface name="{PROPERTIES}"/>'
        f'<interface name="{INTROSPECTABLE}"/></node>'
    )


def _die_with_the_service() -> int:
    """The service's pid, and the kernel told to end this process when it dies."""
    parent = os.getppid()
    try:
        libc = ctypes.CDLL(None, use_errno=True)
        libc.prctl(PR_SET_PDEATHSIG, signal.SIGTERM)
    except (OSError, AttributeError):
        _log.warning("the tray could not ask to end with the service", exc_info=True)
    return parent


def _watcher_here(conn: DBusConnection) -> bool:
    reply = conn.send_and_get_reply(message_bus.NameHasOwner(WATCHER.bus_name), timeout=5)
    return bool(reply.body and reply.body[0])


def _register(conn: DBusConnection, name: str) -> None:
    call = new_method_call(WATCHER, "RegisterStatusNotifierItem", "s", (name,))
    reply = conn.send_and_get_reply(call, timeout=5)
    if reply.header.message_type is MessageType.error:
        raise OSError(f"the tray host refused the item: {reply.body}")


class LinuxTray:
    """The item on the session bus until the service ends this process."""

    def run(self, tray: Tray, *, icon: str) -> int:
        parent = _die_with_the_service()
        try:
            conn = open_dbus_connection("SESSION")
        except (OSError, KeyError, ValueError) as exc:
            _log.info("no session bus for the tray: %s", exc)
            return EXIT_NO_HOST
        with conn:
            if not _watcher_here(conn):
                _log.info("no StatusNotifierItem host on the session bus")
                return EXIT_NO_HOST
            name = f"org.kde.StatusNotifierItem-{os.getpid()}-1"
            conn.send_and_get_reply(message_bus.RequestName(name), timeout=5)
            rule = MatchRule(
                type="signal",
                sender=message_bus.bus_name,
                interface=message_bus.interface,
                member="NameOwnerChanged",
                path=message_bus.object_path,
            )
            rule.add_arg_condition(0, WATCHER.bus_name)
            conn.send_and_get_reply(message_bus.AddMatch(rule), timeout=5)
            _register(conn, name)
            item = Item(tray, icon=icon)
            while os.getppid() == parent:
                try:
                    msg = conn.receive(timeout=60)
                except TimeoutError:
                    continue
                if _watcher_came_back(msg):
                    _register(conn, name)  # the panel restarted: show the item again
                    continue
                for out in item.handle(msg):
                    conn.send(out)
        return 0


def _watcher_came_back(msg: Message) -> bool:
    fields = msg.header.fields
    if msg.header.message_type is not MessageType.signal:
        return False
    if fields.get(HeaderFields.member) != "NameOwnerChanged":
        return False
    body: Sequence[Any] = msg.body
    return len(body) == 3 and body[0] == WATCHER.bus_name and bool(body[2])
