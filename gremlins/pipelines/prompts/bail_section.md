## Bail markers (running under a gremlin pipeline)

If you cannot safely complete your task, write a bail marker before finishing — do not make speculative changes when bailing:

- Task involves **secrets** (credential management, API keys, encryption material): `python -m gremlins.bail secrets "<one-line reason>"`
- Any other reason you cannot proceed: `python -m gremlins.bail other "<one-line reason>"`

Do not write a bail marker if you successfully completed your task — just exit normally.
