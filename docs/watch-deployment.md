# Auto-Clean Deployment Guide

How to run `treehouse watch` as a scheduled cleanup service using your OS's native scheduler.
Treehouse stays a foreground CLI — the OS manages lifecycle, restart, and supervision.

## Prerequisites

```bash
treehouse --version    # v0.2.0+ (P0–P3: GC hardening, watch --interval)
treehouse doctor       # verify pools are healthy before scheduling
```

---

## Linux

### Option A: systemd timer (recommended)

**1. Create the service unit:**

```ini
# /etc/systemd/system/treehouse-watch.service
[Unit]
Description=Treehouse auto-clean sweep
After=network.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/treehouse watch --once --yes
# Run as the user who owns the worktrees, not root.
User=%i
WorkingDirectory=/home/%i
StandardOutput=journal
StandardError=journal
```

**2. Create the timer unit:**

```ini
# /etc/systemd/system/treehouse-watch.timer
[Unit]
Description=Run treehouse auto-clean every 10 minutes

[Timer]
OnBootSec=2min
OnUnitActiveSec=10min
Persistent=true

[Install]
WantedBy=timers.target
```

**3. Enable and start:**

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now treehouse-watch.timer

# Verify it's running
systemctl list-timers | grep treehouse
```

**4. Check last run:**

```bash
journalctl -u treehouse-watch.service --since "1 hour ago"
```

### Option B: cron

```bash
# Edit crontab for the user who owns the worktrees
crontab -e

# Sweep every 10 minutes, suppress output on success
*/10 * * * * treehouse watch --once --yes 2>&1 | logger -t treehouse-watch
```

View logs:

```bash
grep treehouse-watch /var/log/syslog
# or
journalctl -t treehouse-watch
```

---

## macOS

### launchd plist

**1. Create the plist:**

```xml
<!-- ~/Library/LaunchAgents/com.treehouse.watch.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.treehouse.watch</string>

    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/treehouse</string>
        <string>watch</string>
        <string>--once</string>
        <string>--yes</string>
    </array>

    <key>RunAtLoad</key>
    <true/>

    <key>StartInterval</key>
    <integer>600</integer>

    <key>StandardOutPath</key>
    <string>/tmp/treehouse-watch.log</string>

    <key>StandardErrorPath</key>
    <string>/tmp/treehouse-watch.log</string>
</dict>
</plist>
```

**2. Load the agent:**

```bash
launchctl load ~/Library/LaunchAgents/com.treehouse.watch.plist

# Verify
launchctl list | grep treehouse
```

**3. Unload (stop):**

```bash
launchctl unload ~/Library/LaunchAgents/com.treehouse.watch.plist
```

**4. View logs:**

```bash
tail -f /tmp/treehouse-watch.log
```

---

## Windows

### Task Scheduler (GUI)

1. Open **Task Scheduler** (`taskschd.msc`)
2. **Create Basic Task** → Name: `Treehouse Auto-Clean`
3. Trigger: **Daily**, repeat every `10 minutes` for `1 day`
4. Action: **Start a program**
   - Program: `C:\Users\<you>\.cargo\bin\treehouse.exe`
   - Arguments: `watch --once --yes`
5. **Finish** → right-click the task → **Properties**:
   - ☑ Run whether user is logged on or not (optional, for background)
   - ☑ Run with highest privileges only if needed
   - Settings: ☑ Allow task to be run on demand

### Task Scheduler (schtasks CLI)

```powershell
# Create the scheduled task
schtasks /create `
  /tn "Treehouse Auto-Clean" `
  /tr "treehouse watch --once --yes" `
  /sc minute `
  /mo 10 `
  /f

# Check status
schtasks /query /tn "Treehouse Auto-Clean" /v /fo list

# Run manually
schtasks /run /tn "Treehouse Auto-Clean"

# Delete
schtasks /delete /tn "Treehouse Auto-Clean" /f
```

### PowerShell scheduled job (alternative)

```powershell
$trigger = New-JobTrigger -Once -At (Get-Date) `
  -RepetitionInterval (New-TimeSpan -Minutes 10) `
  -RepetitionDuration (New-TimeSpan -Days 9999)

Register-ScheduledJob -Name "TreehouseAutoClean" `
  -Trigger $trigger `
  -ScriptBlock { treehouse watch --once --yes }
```

---

## Configuration

### Interval

Choose based on your agent workload:

| Interval | Best for |
|----------|----------|
| `5m` | High-frequency agents (10+ concurrent) |
| `10m` | Normal agent workflows |
| `30m` | Low-frequency or small repos |
| `1h` | Conservative / disk-space-conscious |

### Dry-run first

Always test with dry-run before enabling real cleanup:

```bash
# See what would be reclaimed (no changes)
treehouse watch --once

# Enable real cleanup
treehouse watch --once --yes
```

### Prune orphans

If you have orphaned worktrees (backing repo deleted):

```bash
treehouse watch --once --yes --prune-orphans
```

---

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| Timer fires but no cleanup happens | `treehouse doctor` — check pool health; `treehouse status` — verify worktrees exist |
| "all worktrees are in use or dirty" | Pool is genuinely full; increase `max_trees` in `treehouse.toml` or `gc` stale trees |
| Logs show "state lock wedged" | Delete the `.lock` file in the pool dir: `~/.treehouse/<pool>/treehouse-state.lock` |
| Timer doesn't fire | `systemctl list-timers` / `launchctl list` / `schtasks /query` — verify scheduler is active |
| Want per-repo scheduling | Use `treehouse watch --once` inside each repo dir (it discovers pools from `$HOME/.treehouse`) |
