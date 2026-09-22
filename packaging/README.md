# Installing the history collector

The collector is what makes the day's trend lines possible: the application
itself only sees the machine while its window is open.

```bash
install -Dm755 target/release/clearview-collector ~/.local/bin/clearview-collector
install -Dm644 packaging/clearview-collector.service \
  ~/.config/systemd/user/clearview-collector.service

systemctl --user daemon-reload
systemctl --user enable --now clearview-collector
```

Check on it:

```bash
clearview-collector --report          # rows and size on disk
systemctl --user status clearview-collector
```

Stop it keeping history at all:

```bash
systemctl --user disable --now clearview-collector
rm -rf ~/.local/share/clearview
```

## What it costs

Measured at steady state, with every retention window full:

| | |
|---|---|
| Database on disk | **~3 MB** |
| Sampling | every 5 seconds |
| Machine history | 5s for 2 hours, 1 min for 2 days, 10 min for a month |
| Per-app history | 1 min for 2 days, 10 min for a month |

Per-app figures are averaged in memory for a whole minute before anything is
written, and only the apps that actually mattered in that minute get a row. The
service runs niced with idle I/O priority, so it yields to whatever you are
doing.
