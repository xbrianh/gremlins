"""Fleet manager package for background gremlins.

Reads every gremlin state file under the per-user state directory from
``gremlins.paths.state_root()``, applies the shared liveness classifier inline,
and prints one scannable line per gremlin. Subcommands (``ack``, ``close``,
``land``, ``log``, ``rm``, ``skip``, ``stop``) operate on a single
gremlin by id-prefix.

Exposed via ``python -m gremlins.cli fleet``.

Exit 0 on the listing path even on unexpected errors: same "never break a
session" principle as the session-summary hook.
"""
