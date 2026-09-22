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

**M1–M3 and M5 complete** — acquisition and the model, verified against real applications.
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

## Layout

| Crate | Role |
|---|---|
| `btm-probe` | Reads `/proc`, `/sys` and cgroups. Knows nothing about apps. |
| `btm-model` | App identity, process roles, subtree aggregation. |
| `btm-store` | SQLite time series for history and trendlines. |
| `btm-collector` | Background sampler, a `systemd --user` service. |
| `btm-app` | Tauri GUI. |

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
