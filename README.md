# simple-tui-apps-dash

A small terminal dashboard for long-running local apps. It watches one
directory, discovers the apps inside it, shows their processes, listening
ports, memory and CPU use, and lets you start, stop and restart them without
leaving the terminal.

```
 RAM 5.2G/15.6G   CPU 23.4%   LOAD 0.42   · resources poll 10s
┌ apps · 3 ────────────────────────────────────────────────────────────┐
│ ▶ web              running  3000     412     2h     84.2M   1.2%  …  │
│   worker           running  -        413     2h     120.4M  0.4%  …  │
│   old-job          stopped  8080?    -       -      -       -     …  │
└──────────────────────────────────────────────────────────────────────┘
┌ system listeners · read-only ────────────────────────────────────────┐
│ PORT    PID      PROCESS              LISTEN                          │
│ 22      812      sshd                 0.0.0.0:22                      │
│ 5432    1104     postgres             127.0.0.1:5432                  │
└──────────────────────────────────────────────────────────────────────┘
```

## Features

- Discovers apps from the direct subdirectories of one root directory.
- Matches processes by working directory and TCP listeners by socket owner.
- Shows per-app RAM (summed RSS) and CPU, plus system RAM, CPU and load.
- Starts, stops and restarts apps from the keyboard.
- Streams app output to `<app>/logs/app.log`.
- Resource usage is sampled every 10 seconds; app/listener state every 2 seconds.

## Requirements

- Linux (`/proc` is used for process, memory and socket data).
- Rust 1.85 or newer.

## Build and install

```sh
cargo build --release
install -Dm755 target/release/appdash ~/.local/bin/appdash
```

Or link the binary instead of copying it:

```sh
ln -sf "$PWD/target/release/appdash" ~/.local/bin/appdash
```

Make sure `~/.local/bin` is on your `PATH`.

## Usage

```sh
APPDASH_DIR=/srv/apps appdash
```

`APPDASH_DIR` defaults to `/root/running`. Every direct subdirectory is shown
as an app; hidden directories and `logs` are skipped.

### app.toml

Each app can include an optional `app.toml`:

```toml
name = "web"          # display name, defaults to the directory name
cmd = "./run.sh"      # shell command used by start/restart
port = 3000           # expected port, shown as "3000?" while none is listening
```

`cmd` is optional, but start/restart require it. It runs with the app
directory as its working directory:

```sh
setsid -f sh -c 'echo $$ > .appdash.pid; exec sh -c "<cmd>"'
```

A pid file is written to `<app>/.appdash.pid`; logs go to
`<app>/logs/app.log`.

### Keys

| Key | Action |
| --- | --- |
| `↑` / `↓`, `k` / `j` | Move selection |
| `Tab` | Switch focus between apps and listeners |
| `s` | Start selected app |
| `x` | Stop selected app |
| `r` | Restart selected app |
| `R` | Refresh state and resource sample |
| `q`, `Esc`, `Ctrl-C` | Quit |

The listeners pane is read-only and lists TCP sockets that do not belong to
an app in the watched directory.

## Resource metrics

- Per-app RAM is the sum of `VmRSS` for all processes whose working directory
  is inside the app directory.
- Per-app CPU is the sum of process CPU time deltas between samples; multiple
  threads can exceed 100%.
- Header RAM is `MemTotal - MemAvailable`, CPU comes from `/proc/stat`, and
  load is the 1-minute `/proc/loadavg` value.

## Development

```sh
cargo fmt
cargo clippy
cargo build --release
```

## License

MIT — see [LICENSE](LICENSE).
