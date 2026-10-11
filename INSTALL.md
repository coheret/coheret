# Installing coheret

## Prerequisites

- A working `docker` CLI on `PATH` and a reachable Docker engine
  (local socket, Docker Desktop, or a remote context via `DOCKER_HOST` /
  `docker context`).
- A Rust toolchain if you are building from source.

## Install

From crates.io:

```sh
cargo install coheret
```

From source:

```sh
git clone https://github.com/coheret/coheret
cd coheret
cargo install --path crates/coheret
```

Verify the installation:

```sh
coheret doctor
```

`doctor` checks the docker CLI, the daemon connection, and that coheret can
write its configuration files.

## Upgrading

Re-run the same install command. User configuration (`probes.yaml`,
`schedules.yaml`) and runtime state (`probe-state.yaml`) in the per-user
config directory are preserved.

## Uninstalling

```sh
cargo uninstall coheret
```

Config and state files are left behind; delete the config directory if you
want them gone:

- Windows: `%APPDATA%\coheret`
- macOS: `~/Library/Application Support/coheret`
- Linux: `~/.config/coheret`

## Running the daemon persistently

Schedules and probes are only enforced while `coheret daemon` runs. Some
options per platform:

### systemd user unit (Linux)

```ini
# ~/.config/systemd/user/coheret.service
[Unit]
Description=coheret container supervisor
After=docker.service

[Service]
ExecStart=%h/.cargo/bin/coheret daemon --interval 30 --auto-heal
Restart=on-failure

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now coheret
```

### launchd (macOS)

```xml
<!-- ~/Library/LaunchAgents/dev.coheret.daemon.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.coheret.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/coheret</string>
    <string>daemon</string>
    <string>--interval</string><string>30</string>
    <string>--auto-heal</string>
  </array>
  <key>KeepAlive</key><true/>
</dict>
</plist>
```

```sh
launchctl load ~/Library/LaunchAgents/dev.coheret.daemon.plist
```

### Windows (Task Scheduler)

```powershell
schtasks /create /tn "coheret daemon" /sc onlogon `
  /tr "C:\Users\<you>\.cargo\bin\coheret.exe daemon --interval 30 --auto-heal"
```

## Next steps

- `coheret status` — health overview
- `coheret probe apply coheret.yaml` — configure probes from a YAML policy
  (see [README.md](README.md#probes))
- `coheret schedule add web "0 3 * * *"` — nightly restarts
