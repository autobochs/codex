# codex-relay

`codex-relay` bridges ChatGPT remote control to an unmodified Codex app-server
subprocess. Relay credentials default to
`~/.codex-relay`; the child app-server inherits the caller's normal
`CODEX_HOME`.

```console
codex-relay login
codex-relay remote-control pair
codex-relay remote-control start
```

Use `--session-codex-home` to select the child app-server account explicitly,
or `--codex` to run a particular Codex executable. The relay starts one child
app-server and maps every remote logical client to a separate Unix-socket
connection, while Codex multiplexes all threads in the child process.

The relay owns only remote-control authentication and enrollment state. It
does not copy credentials into the child process. If `--session-codex-home` is
omitted, the child uses the same `CODEX_HOME` (or default `~/.codex`) as any
other invocation of the selected `codex` executable.

The relay exits if the child app-server or remote-control transport stops, and
shuts the child down on Ctrl-C. A failure to establish one logical client
connection is isolated to that client.
