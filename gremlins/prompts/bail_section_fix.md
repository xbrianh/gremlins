## Bail markers (running under a gremlin pipeline)

If you cannot fix the failure — for example, the check reports a violation you legitimately cannot resolve — end your final message with this line and nothing after it:

```
BAIL: other: <one-line reason>
```

Do not write a bail marker if you successfully fixed the failure — just exit normally.
