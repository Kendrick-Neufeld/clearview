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

**M1 complete** — the acquisition layer, verified against known loads.

```bash
cargo build --release
./target/release/btm-dump --interval 2 --top 15
./target/release/btm-dump --match vesktop     # one app, added up honestly
```

## Layout

| Crate | Role |
|---|---|
| `btm-probe` | Reads `/proc`, `/sys` and cgroups. Knows nothing about apps. |
| `btm-model` | App identity, process roles, subtree aggregation. *(M2)* |
| `btm-store` | SQLite time series for history and trendlines. *(M5)* |
| `btm-collector` | Background sampler, a `systemd --user` service. *(M5)* |
| `btm-app` | Tauri GUI. *(M3+)* |

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
