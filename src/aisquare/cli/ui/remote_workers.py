"""The names of Remote's UI workers, apart from Remote (#46).

The quit and the hand-over list every UI worker by name (``cli.ui.app._writing_workers`` and
``_waited_reads``), Remote's two among them. Read from ``views.settings_remote``, every quit
loaded Remote, while it was off and nothing of it had run. Here they cost nothing.
"""

from __future__ import annotations

TOKEN_WORKER = "settings-remote-token"
"""The worker that saves a pasted ngrok token (``views.settings_remote.store_ngrok_token``)."""
CHECK_WORKER = "settings-remote-check"
"""The worker that looks at what Remote needs (``views.settings_remote.remote_needs``)."""
RESTORE_WORKER = "remote-restore-after-stop"
"""The worker that waits, Remote turned off and on again at once, for the one still stopping
to be off and its writes landed before the new one comes up (``cli.ui.app``)."""
