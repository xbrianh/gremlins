## Bail markers (running under a gremlin pipeline)

If you cannot safely complete your task, end your final message with a single line in this exact format and nothing after it — do not make speculative changes when bailing:

- Task involves **secrets** (credential management, API keys, encryption material): `BAIL: secrets: <one-line reason>`
- Any other reason you cannot proceed: `BAIL: other: <one-line reason>`

Do not write a bail marker if you successfully completed your task — just exit normally.
