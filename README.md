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

`azpect` uses the `DefaultAzureCredential` from Microsoft's official Rust
SDK (`azure_identity` 0.27). At this SDK version the chain consists of
the **Azure CLI** (`az login`) and **Azure Developer CLI** (`azd auth
login`), in that order — environment-variable, workload-identity, and
managed-identity links are not yet ported to the Rust SDK. For most
users the easiest path is `az login`.

Run `azpect debug-auth` to confirm credentials resolve and to print the
list of subscriptions the active credential can see.

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

## Releasing

1. Bump `version` in `Cargo.toml`.
2. `cargo update -p azpect` to refresh `Cargo.lock`.
3. Commit on `main` (e.g. `chore: release vX.Y.Z`).
4. `git tag vX.Y.Z && git push --tags` — the GitHub Actions release workflow
   builds Linux x86_64 and macOS (aarch64 + x86_64) archives, creates a
   GitHub Release, and publishes to crates.io.

Tags containing a hyphen (e.g. `v0.2.0-rc1`) are marked as prereleases.

## License

MIT OR Apache-2.0.
