"""Loaded by every Python child that inherits the suite's environment: conftest puts this
directory first on ``PYTHONPATH`` (``tests/conftest.py``'s ``keychain_guard``).

It installs ``rlx_keychain_guard``'s audit hook, so a child can never run the real macOS
Keychain tool, another credential store's tool or (outside the native suites) a real agent
binary, nor open the developer's real home, which the environment names
(``RLX_TESTS_GUARDED_PATHS``, #54). It points the child's ``claude_accounts.KEYCHAIN_TOOL`` at
the fake named by ``RLX_TESTS_KEYCHAIN_TOOL`` when a test gives it one. Not a test module.
"""

import importlib.util
import os
import sys
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    "rlx_keychain_guard", Path(__file__).with_name("rlx_keychain_guard.py")
)
if _spec is not None and _spec.loader is not None:
    guard = importlib.util.module_from_spec(_spec)
    sys.modules["rlx_keychain_guard"] = guard
    _spec.loader.exec_module(guard)
    guard.install()
    fake = os.environ.get(guard.FAKE_VAR)
    if fake:
        try:
            from aisquare.core import claude_accounts
        except ImportError:  # a child that doesn't run rlx
            pass
        else:
            claude_accounts.KEYCHAIN_TOOL = fake
