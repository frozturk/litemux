# litemux

litemux is a small terminal session server. It runs programs in pseudo-terminals in a background process, so they keep running when the app showing them quits. Clients attach to a session, get its current screen and scrollback, and then stream its output live.

It is built for [vide](https://github.com/frozturk/vide) and does far less than a terminal multiplexer: there are no windows, panes, key bindings or status bar. Output is passed through unchanged, so the attached terminal (xterm.js in vide) does its own scrolling, selection and search.

## Status

| Platform | State |
|---|---|
| macOS | in daily use |
| Linux | expected to work, untested |
| Windows | builds on ConPTY, untested |

## Build

```sh
cargo build --release
cargo test
```

The binary is `target/release/litemux`.

## Usage

Every command takes the server file with `-S FILE`, or from the `LITEMUX` environment variable.

```sh
litemux -S ~/litemux.json server     # run the server (it stays in the foreground; start it detached)
litemux -S ~/litemux.json ls         # name, pid, state and title of every session
litemux -S ~/litemux.json kill NAME  # hang up a session
litemux state busy                   # inside a session: set its state string
```

The server listens on a random port on `127.0.0.1`, then writes the port, a random 16-byte token and its pid to the server file, readable only by you. Every request must carry the token. A second server started with the same file exits if the first one still answers. The server exits and deletes the file when its last session ends.

Programs started in a session get these environment variables:

| Variable | Value |
|---|---|
| `LITEMUX` | the server file |
| `LITEMUX_SESSION` | the session name |
| `LITEMUX_BIN` | the litemux binary, so hooks can call `"$LITEMUX_BIN" state …` |

## Protocol

A client opens a TCP connection and sends one JSON line. Every request includes `"token"`.

| Request | Reply |
|---|---|
| `{"op":"spawn","name","cwd","argv":[…],"env":{…},"cols","rows"}` | `{"ok":true}`; replaces a session with the same name |
| `{"op":"list"}` | `{"ok":true,"sessions":[{"name","pid","title","state"}]}` |
| `{"op":"kill","name"}` | `{"ok":true}` |
| `{"op":"state","name","value"}` | `{"ok":true}` |
| `{"op":"attach","name"}` | `{"ok":true}`, then the connection switches to frames |

Errors come back as `{"ok":false,"error":"…"}` and the server closes the connection.

After an attach, both sides send frames: one kind byte, a big-endian `u32` payload length, then the payload.

| Direction | Kind | Payload |
|---|---|---|
| server → client | `D` | terminal output |
| server → client | `X` | exit code as decimal text, sent before the connection closes |
| client → server | `I` | input bytes |
| client → server | `R` | `u16` columns, `u16` rows, both big-endian |

The first `D` frame is a snapshot: a terminal reset, up to 5000 lines of scrollback, then the visible screen, cursor and input modes (bracketed paste, mouse tracking, application keys, alternate screen). Closing the connection detaches; the session keeps running.

Several clients can attach to one session. Output goes to all of them. Terminal query replies (cursor position, device attributes, colour reports) are only forwarded from the client that attached or typed most recently, so the program does not get duplicates. While no client is attached, the server answers cursor position, status and device attribute queries itself.

The session title comes from the program's OSC 0/2 title sequences. The state is a free-form string that clients set with `state`; vide uses it for agent status from Claude Code and Codex hooks.

## License

MIT
