# bettertaskmanager

A Linux task manager built around one idea: the answer to *"what is my computer
doing?"* should be visible, not derived.

Existing monitors are not just plain — they are measurably wrong in ways that make
them hard to read:

- **Apps are split apart.** Processes that escape their launcher's cgroup land in the
  bare session scope, so one app shows up as several unrelated entries sharing a name.
- **Memory is over-reported.** Summing RSS across a multi-process app double-counts
  every shared page. Measured here: one app reporting 2.99 GB actually costs 1.34 GB.
- **Parents look idle.** A launcher shows 0% while its child burns a core, because the
  headline figure is per-process rather than per-subtree.
- **Sub-processes never explain themselves.** The information is right there in the
  command line; nothing surfaces it.

## Status

**M1–M3, M5 and M6 complete** — acquisition and the model, verified against real applications.
On this machine 377 processes resolve to 61 apps.

```bash
cargo build --release

./target/release/btm-apps                     # apps, busiest first
./target/release/btm-apps --app vesktop       # one app, explained
./target/release/btm-dump  --match vesktop    # the raw numbers underneath
```

```
  Vesktop                       2.9%    1126 MB   8 processes
      window   (2) Discord | Lobby | Broddars
      memory   1126 MB actual — tools that sum RSS would say 1797 MB (1.6x)
      3929    (2) Discord | Lob…   2.9% (0.6% own) Main process
        3946    vesktop              2.0% (0.0% own) Process template
                ↳ idle itself; the work is in the 2 processes below it
          3948    vesktop              2.0% (0.0% own) Process template
            4160    vesktop              2.0%           Window or tab
        3996    vesktop              0.0%           Network
        4236    vesktop              0.4%           Audio
```

## Building and checking

```bash
cargo test --release      # 34 tests across the Rust crates
./check-ui.sh             # parses every UI module, lints references, runs 40 tests
cargo build --release
```

`check-ui.sh` exists because a syntax error or a deleted helper in the
interface takes the whole page down before a line of it runs — no handler can
catch that, and on screen it is indistinguishable from a backend failure. The
reference lint in particular was written after a refactor removed six functions
and left their callers behind; it finds that in a second rather than in a
screenshot.

## Distributing

```bash
NO_STRIP=1 cargo tauri build --bundles appimage,deb
```

`NO_STRIP=1` is required on Arch: the `strip` bundled inside linuxdeploy is too
old to read the `.relr.dyn` sections modern libraries use, and fails the build
without it.

For Arch, `packaging/arch/` holds a PKGBUILD ready for the AUR — see the README
beside it. An AppImage built here needs glibc 2.39 or newer, because it links
against this machine's libraries; building it on the oldest distribution you
intend to support is the only way round that.

## Installing

```bash
cargo build --release
./packaging/install.sh
```

Puts `clearview` on your PATH, adds it to your application menu with its icon,
and enables the background collector. Everything lands under `$HOME`; nothing
needs root. The binary is named `clearview` because that name becomes the
Wayland app id, which is what a launcher matches against `StartupWMClass` — with
any other name the window gets no icon anywhere.

## Layout

| Crate | Role |
|---|---|
| `btm-probe` | Reads `/proc`, `/sys` and cgroups. Knows nothing about apps. |
| `btm-model` | App identity, process roles, subtree aggregation. |
| `btm-store` | SQLite time series for history and trendlines. |
| `btm-collector` | Background sampler, a `systemd --user` service. |
| `btm-app` | Tauri GUI: applications view and performance view. |

## What is measured, and how

| | Source | Note |
|---|---|---|
| Per-core load | `/proc/stat` | per logical CPU |
| Per-core temperature | `coretemp` hwmon | per *physical* core; hyper-threads share one |
| Clock speed | `cpufreq` | per logical CPU |
| Package power | Intel RAPL | energy counter differenced, wrap handled |
| Built-in GPU | DRM fdinfo, summed per client | a floor, not a measurement — work the driver cannot attribute to a client is invisible |
| Discrete GPU | `nvidia-smi` | sampled every few seconds, not every tick |
| Per-app GPU | DRM fdinfo, keyed by client id | keyed by client, not pid, or a browser's use multiplies by its tab count |
| Network, machine-wide | `/proc/net/dev` | counts everything |
| Network, per app | `ss`, per-socket TCP counters | **TCP only** — UDP keeps no such counter, so QUIC is invisible and per-app sums read lower than the total |
| Disk, machine-wide | `/proc/diskstats` | whole devices only; partitions carry overlapping counters |
| Disk, per app | `/proc/PID/io` | what the program asked the kernel for, which is not what the drive did — cached reads never reach it |

## Closing an application

The only irreversible thing here, so it is built to be hard to get wrong:

- **Two steps.** The first press states the consequence; the second carries it out.
- **Asked, not forced.** `SIGTERM` first, so a program can save and exit properly.
  Forcing is offered only after asking has visibly failed, and says plainly that
  nothing will be saved.
- **Addressed by identity, not by number.** Pids are reused. Between a row being
  drawn and a click landing, a process can exit and its number be handed to
  something else — so the start time is re-read immediately before the signal and
  must still match. It does not, nothing is sent.
- **Consequences in the app's own terms.** Closing your compositor says it will
  end your session and shut every window, rather than expecting you to recognise
  a process name.
- Kernel threads and init are refused outright.

## Reading a list that will not sit still

Sorting by processor use means the rows resequence every second, so the row you
are reaching for slides out from under the cursor — and the treemap does the
same thing with its blocks. Both hold their arrangement while the pointer is
inside them: the numbers keep updating, the geometry does not move. Filtering by
name and sorting by memory or name are there for when you know what you are
looking for.

## Two layouts

Stacked puts the blocks above the list, which suits a wide screen. Side by side
puts the list on the left and the blocks on the right, which suits a portrait
one — or simply reading preference. Below 760px wide the stacked arrangement is
used regardless, because two columns that narrow serve neither.

## The two kinds of graph

They answer different questions, and neither can answer the other's:

- **Live** — per-second, straight from the running app, about five minutes deep.
  The only place a short burst is visible at full detail, and the only place a
  spike can be attributed to the exact second it happened.
- **Hour / Day / Week / Month** — from the collector's database, which stores at
  best five-second detail and coarsens with age. This is the only thing that can
  tell you what happened while the window was shut.

## What it costs

Measured, not estimated. The collector runs all day for a graph nobody is
watching right now, so it is built to disappear:

| | |
|---|---|
| Collector CPU | **0.044%** of a 16-thread machine (0.7% of one core) |
| Collector memory | **3.8 MB** (PSS) |
| History on disk | **3.12 MB** at full steady state — a month of history |

The size is held down by storing per-app figures only after averaging them in
memory for a whole minute, keeping just the apps that mattered in that minute,
scaling every value to a small integer instead of a float, and folding old
samples into coarser buckets as they age. See `packaging/README.md` to install
the service.

## Measurement rules

These are deliberate, and they are why the numbers here differ from other tools:

- **CPU is a percentage of the whole machine** by default, with the thread count always
  shown. Per-core percentage is available but never silently mixed in.
- **`iowait` counts as idle.** The CPU was free; nothing was runnable.
- **Memory totals use PSS**, never summed RSS, wherever more than one process is
  involved.
- **Pressure (PSI) is reported alongside utilisation**, because busy and struggling are
  different states and only one of them is a problem.
- **Rates are `None`, not `0`, until two samples exist.** The tool says it does not know
  yet rather than reporting a confident zero.
