# codex-relay

`codex-relay` bridges ChatGPT remote control to an unmodified Codex app-server
subprocess. Relay credentials and enrollment state use the regular
`CODEX_HOME` (`~/.codex` by default). The child app-server also inherits the
caller's normal `CODEX_HOME`.

```console
codex-relay login
codex-relay remote-control pair
codex-relay remote-control start
```

Login uses device-code authentication by default. Pass `--browser` to use the
localhost browser-callback flow instead.

Pass `--verbose` to any command to print transport, enrollment, and pairing
diagnostics. `RUST_LOG` can be used to supply a custom tracing filter.

Use `--relay-home` (or `CODEX_RELAY_HOME`) to give the relay a separate Codex
home. Use `--codex-home` to select the child app-server's Codex home.

Use `--codex-home` to select the child app-server account explicitly,
or `--codex` to run a particular Codex executable. Use `--name` to control the
machine name shown to remote clients. Pairing always produces a short manual
code after the remote-control websocket is connected. The relay starts one child
app-server and maps every remote logical client to a separate Unix-socket
connection, while Codex multiplexes all threads in the child process.

The relay owns only remote-control authentication and enrollment state. It
does not copy credentials into the child process. If `--codex-home` is
omitted, the child uses the same `CODEX_HOME` (or default `~/.codex`) as any
other invocation of the selected `codex` executable.

The relay exits if the child app-server or remote-control transport stops, and
shuts the child down on Ctrl-C. A failure to establish one logical client
connection is isolated to that client.
