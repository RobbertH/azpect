# azpect

A terminal UI for observing the health of Azure APIs at a glance.

`azpect` lists Function Apps, API Management instances, and Container Apps across
every subscription you can access, and shows a health badge plus
24-hour / 7-day sparklines for **requests**, **HTTP 5xx errors**,
**CPU**, and **memory** without leaving your terminal. For Function Apps
and Container Apps it also tails the most recent logs (with an
"errors only" filter) via Log Analytics.

## Status

Pre-alpha. Build is in progress.

## Authentication

`azpect` uses Azure's `DefaultAzureCredential` chain — environment
variables, workload identity, managed identity, the Azure CLI
(`az login`), Azure PowerShell, then `azd`. Whichever resolves first
wins. For most users the easiest path is `az login`.

## Build

```sh
cargo build --release
./target/release/azpect
```

## Configuration

State (favorites, last subscription, theme, default time window) is
stored under `${XDG_CONFIG_HOME:-~/.config}/azpect/config.toml`. The
file is created on first run; nothing is written to the repo.

## Keys

Vim-style navigation throughout. Single-letter shortcuts for actions; uppercase
or chords avoid clobbering cursor movement.

| Key                  | Action                                         |
|----------------------|------------------------------------------------|
| `h` `j` `k` `l`      | Move cursor (left / down / up / right)         |
| `g g` / `G`          | Top / bottom of list                           |
| `Ctrl-d` / `Ctrl-u`  | Half page down / up                            |
| `Tab` / `Shift-Tab`  | Cycle between panels                           |
| `Enter`              | Open detail / expand selected                  |
| `L`                  | Open logs (Function App / Container App)       |
| `e`                  | In logs view: errors-only toggle               |
| `/`                  | Search / filter resources                      |
| `f`                  | Toggle favorite on selected                    |
| `s`                  | Switch subscription                            |
| `r`                  | Refresh                                        |
| `d` / `w`            | Day / week time window                         |
| `?`                  | Help overlay                                   |
| `q` / `Esc`          | Back / quit                                    |

## License

MIT OR Apache-2.0.
