# Clearview on niri — diagnosis handoff

You are working on a machine running **niri**. Clearview was installed there and
sat on "Measuring…" forever without ever showing data. This document is the
context for that, written by the session that built the project.

Read the "Most likely cause" section first — a fix for it has already been
pushed, and confirming or eliminating it should take five minutes.

---

## What Clearview is

A Linux system monitor that groups processes into *applications* rather than
listing process names, and explains what each process in an application is for.
Rust workspace, Tauri v2 shell, plain-JS frontend (no bundler, no npm
dependencies).

```
crates/btm-probe/      reads /proc, /sys, cgroups, DRM fdinfo, compositor IPC
crates/btm-model/      app identity, process roles, aggregation
crates/btm-store/      SQLite history
crates/btm-collector/  background sampler -> systemd --user service
crates/btm-app/        Tauri backend; src/main.rs holds the IPC commands
ui/                    frontend: app.js, chart.js, treemap.js, lib.js
```

Two binaries: `clearview` (the window) and `clearview-collector` (history).

---

## The symptom

The window opens. The summary bar shows dashes, the treemap shows "Measuring…",
and it never changes.

That string is the treemap's placeholder. It is replaced the first time the
frontend receives a snapshot. So the symptom means **no snapshot ever arrived**
— not that the data was wrong.

The chain is: a background thread in `crates/btm-app/src/main.rs` (`sample_loop`)
samples once a second, builds a `SystemModel`, and emits it as a `snapshot`
event. The frontend's `apply()` draws it. If the loop never completes a pass, or
the event never lands, you get exactly this.

## Most likely cause — already fixed, please verify

`wm::windows()` asks the compositor which process owns which window. It is
called from inside the sampling loop.

Until commit `9b23826` it read replies with `read_to_string`, which reads **until
the peer closes the connection**. Hyprland answers and hangs up, so it worked
there. **niri keeps the connection open** — the same socket carries event
streams — so reading to EOF waits for something that never arrives.

That would hang the sampling loop on its first pass, before a single snapshot,
which matches the symptom exactly.

Reads are now bounded by a newline, EOF, or a 400 ms timeout
(`crates/btm-probe/src/wm.rs`, `read_reply`).

**To verify:**

```bash
git pull
cargo build --release
./target/release/btm-apps --interval 2 --top 8
```

`btm-apps` is a CLI that prints the same model the window draws. If it prints
applications with window titles beside them, the hang is fixed and the window
should work too. If it hangs, it is still stuck — see below.

Also worth running:

```bash
./target/release/btm-dump --devices --interval 2
```

## If it still hangs

Find where. The loop is one thread; a stuck one is visible:

```bash
./target/release/btm-apps --interval 2 --top 5 &
sleep 5
PID=$!
cat /proc/$PID/task/*/stack 2>/dev/null | head
cat /proc/$PID/wchan; echo
ls -l /proc/$PID/fd | grep -i sock
```

Or reach for `strace -f -e trace=network,read,write ./target/release/btm-apps`
and look for a `read` that never returns.

Talk to niri directly to see what its socket actually does:

```bash
echo '"Windows"' | timeout 3 socat - UNIX-CONNECT:$NIRI_SOCKET | head -c 400
```

If that returns promptly, the protocol is fine and the bug is in our handling of
it. If it hangs, note whether a newline arrives before it stalls — that is the
assumption `read_reply(stream, true)` rests on.

## If it runs but shows everything as "Background"

Different problem, and expected if the niri query returns nothing. Since
commit `41af34d` the interface says so explicitly — a banner reading "No
compositor could be queried for window information". If you see that banner,
`wm::windows()` is returning empty and the place to look is
`crates/btm-probe/src/wm.rs`:

- `niri_socket()` — finds the socket via `NIRI_SOCKET` or by scanning
  `$XDG_RUNTIME_DIR` for `niri*.sock`. Check the real filename on this machine;
  the pattern may be wrong.
- `niri_windows()` — expects `{"Ok":{"Windows":[...]}}` with each window
  carrying `pid`, `app_id`, `title`, `is_focused`. If niri's shape differs, or
  `pid` is absent in this version, windows are skipped. **Print the raw reply
  before assuming.**

None of the niri code was ever run against niri. It was written from the
protocol description by someone who only had Hyprland. Treat every assumption in
it as unverified.

## Why window data matters more than it looks

It is what separates an application from a background process, and what stops a
program the compositor launched from being filed *under* the compositor.

That second part caused a serious incident: on niri, with no window data and the
compositor running as a systemd service, everything niri launched inherited its
cgroup. Processes sharing a service cgroup are grouped into one application — so
the compositor and every program started from it became a single entry, and
"close this app" sent SIGTERM to niri. A user lost their session that way.

Guards now make that unreachable (`crates/btm-probe/src/control.rs`): a
compositor is never signalled, and neither is any process Clearview descends
from. **Do not weaken those.**

## Conventions

- `./check-ui.sh` before any commit touching `ui/` — it parses every module,
  runs a reference lint, and runs the JS tests. A syntax error or a deleted
  helper in the frontend takes the whole page down before a line runs, and no
  handler can catch it. This has bitten twice.
- `cargo test --release` and `cargo clippy --release --all-targets` are expected
  to be clean.
- Prefer a test that reproduces the fault over a manual check. The read-timeout
  fix has one: a server that replies and then deliberately never closes.
- Commit messages explain *why*, and say plainly when something was wrong.

## Useful facts

- History lives at `~/.local/share/clearview/history.db`. If the collector was
  hanging too, it will be empty or stale — check
  `systemctl --user status clearview-collector`.
- The frontend surfaces uncaught errors in a banner at the top of the window.
  A blank-looking failure with no banner means the JS never ran at all,
  usually a parse error.
- `btm-apps` and `btm-dump` (in `target/release/` after a build) print the model
  and the raw sensor readings respectively. Reach for them before the GUI.
- Building from scratch takes about five minutes.

## The actual question

Does `wm::windows()` return windows on niri, and does the sampling loop complete
a pass? Everything else follows from those two answers.
