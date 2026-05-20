# azpect

A terminal UI for observing the health of Azure APIs at a glance.
Inspired by [k9s](https://github.com/derailed/k9s) and [flowrs](https://github.com/jvanbuel/flowrs), with vim-like keybindings.

`azpect` lists Function Apps, API Management instances, Container Apps, and Application Gateways across every subscription you can access.
For Function Apps and Container Apps it also tails the most recent logs (with an "errors only" filter) via Log Analytics.

## Install

### Homebrew (macOS, Linux)

```sh
brew install RobbertH/tap/azpect
```

Or tap once and install by name:

```sh
brew tap RobbertH/tap
brew install azpect
```

Upgrade later with `brew upgrade azpect`.

## Configuration

State (favorites, last subscription, theme, default time window) is
stored under `${XDG_CONFIG_HOME:-~/.config}/azpect/config.toml`. The
file is created on first run.

## License

MIT OR Apache-2.0.
