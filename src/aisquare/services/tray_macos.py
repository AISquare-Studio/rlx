"""The tray's macOS half (#34): rlx's glyph in the menu bar, an ``NSStatusItem`` and its menu.

Through pyobjc's AppKit, in the ``tray`` extra, as an accessory app: no Dock icon and no menu
of its own. A status item with a menu opens it on any click, and the menu bar has no
double-click, so *Open rlx* is the menu's first item (the ruling on #34, Q2). The menu is
rebuilt when it is about to open (``menuNeedsUpdate:``), from the service's status: nothing
polls while it is closed. *Quit rlx…* asks in an alert that names the agents it leaves.

The glyph is ``core.glyph``'s PNG at 18 pt, 1x and 2x, marked a template when ``[tray] icon``
says so (macOS then paints it as its other menu bar icons).

The service ends this process with ``SIGTERM``, whose default ends it at once; a check every
:data:`PARENT_SECONDS` ends it too if the service died without one. In the login session only:
a service started over SSH has no menu bar, and starts no tray (``service_tray.why_no_tray``).

Imported by ``services.tray.backend_for`` alone, in the tray's process. The suite imports it
where pyobjc is installed, to build its menu and image in memory and check its parent; no test
starts its status item or its event loop (``MacTray.run``).
"""

from __future__ import annotations

import os
from typing import Any

from AppKit import (
    NSAlert,
    NSAlertFirstButtonReturn,
    NSApplication,
    NSApplicationActivationPolicyAccessory,
    NSBitmapImageRep,
    NSImage,
    NSMenu,
    NSMenuItem,
    NSStatusBar,
    NSVariableStatusItemLength,
)
from Foundation import NSData, NSObject, NSTimer
from PyObjCTools import AppHelper

from aisquare.core import glyph
from aisquare.core.version import PRODUCT_NAME
from aisquare.services.tray import QUIT, QUIT_NOW, MenuItem, Tray

POINTS = 18
"""The glyph's height in the menu bar, in points: drawn at 1x and 2x."""
PARENT_SECONDS = 30.0


def menu_bar_image(icon: str) -> Any:
    """``[tray] icon`` as an ``NSImage`` of :data:`POINTS` points, from ``core.glyph``'s PNGs."""
    choice = glyph.icon_named(icon)
    small = glyph.icon(choice, POINTS)
    image = NSImage.alloc().initWithSize_((small.width, small.height))
    for scale in (1, 2):
        drawn = glyph.icon(choice, POINTS * scale)
        data = drawn.png()
        rep = NSBitmapImageRep.imageRepWithData_(NSData.dataWithBytes_length_(data, len(data)))
        rep.setSize_((small.width, small.height))
        image.addRepresentation_(rep)
    image.setTemplate_(choice == "template")
    return image


def menu_item(row: MenuItem, target: Any) -> Any:
    """``row`` as an ``NSMenuItem`` whose click goes to ``target``'s ``clicked:``. A function,
    not a method of :class:`_Menu`: pyobjc takes each of an ``NSObject``'s methods for a
    selector, and one with an argument but no trailing ``_`` fails the class at import."""
    if row.separator:
        return NSMenuItem.separatorItem()
    selector = "clicked:" if row.action is not None else None
    item = NSMenuItem.alloc().initWithTitle_action_keyEquivalent_(row.label, selector, "")
    item.setTarget_(target)
    item.setRepresentedObject_(row.action)
    item.setEnabled_(row.action is not None or bool(row.children))
    if row.children:
        sub = NSMenu.new()
        sub.setAutoenablesItems_(False)
        for child in row.children:
            sub.addItem_(menu_item(child, target))
        item.setSubmenu_(sub)
    return item


def asked(tray: Tray) -> bool:
    """*Quit rlx…*'s alert: whether the person chose to quit."""
    title, detail = tray.question()
    alert = NSAlert.new()
    alert.setMessageText_(title)
    alert.setInformativeText_(detail)
    alert.addButtonWithTitle_(f"Quit {PRODUCT_NAME}")
    alert.addButtonWithTitle_("Cancel")
    NSApplication.sharedApplication().activateIgnoringOtherApps_(True)
    return bool(alert.runModal() == NSAlertFirstButtonReturn)


class _Menu(NSObject):  # type: ignore[misc]
    """The menu's delegate and its rows' target."""

    def menuNeedsUpdate_(self, menu: Any) -> None:
        menu.removeAllItems()
        for row in self.tray.items():
            menu.addItem_(menu_item(row, self))

    def clicked_(self, sender: Any) -> None:
        action = sender.representedObject()
        if action == QUIT:
            if asked(self.tray):
                self.tray.act(QUIT_NOW)
            return
        if action:
            self.tray.act(str(action))

    def check_(self, _timer: Any) -> None:
        if os.getppid() != self.parent:  # the service died without stopping this tray
            AppHelper.stopEventLoop()


def delegate_for(tray: Tray) -> Any:
    """A :class:`_Menu` for ``tray``. ``new``, not ``alloc().init()``: the config-write guard
    reads calls by name (tests/test_config_writes_stay_in_the_cli.py), and a call named
    ``init`` would join every ``run`` to the CLI's ``init``, which writes the config."""
    delegate = _Menu.new()
    delegate.tray = tray
    delegate.parent = tray.service_pid or os.getppid()
    return delegate


class MacTray:
    """The status item until the service ends this process."""

    def run(self, tray: Tray, *, icon: str) -> int:
        app = NSApplication.sharedApplication()
        app.setActivationPolicy_(NSApplicationActivationPolicyAccessory)
        delegate = delegate_for(tray)
        item = NSStatusBar.systemStatusBar().statusItemWithLength_(NSVariableStatusItemLength)
        button = item.button()
        button.setImage_(menu_bar_image(icon))
        button.setToolTip_(PRODUCT_NAME)
        menu = NSMenu.new()
        menu.setAutoenablesItems_(False)
        menu.setDelegate_(delegate)
        item.setMenu_(menu)
        NSTimer.scheduledTimerWithTimeInterval_target_selector_userInfo_repeats_(
            PARENT_SECONDS, delegate, "check:", None, True
        )
        AppHelper.runEventLoop()
        return 0
