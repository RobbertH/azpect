# azpect

A terminal UI for inspecting the health of Azure APIs at a glance.
Inspired by [k9s](https://github.com/derailed/k9s) and [flowrs](https://github.com/jvanbuel/flowrs), with vim-like keybindings.

`azpect` lists Function Apps, API Management instances, Container Apps, and Application Gateways across every subscription you can access. It also drills into Storage accounts (containers / blobs), Container Registries (repositories / tags), Cosmos DB (databases / containers / item preview), and Key Vaults (secrets + certificates, metadata only).

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
stored under `${XDG_CONFIG_HOME:-~/.config}/azpect/config.toml`.
The file is created on first run.

## Authentication

`azpect` uses the [azure_identity](https://docs.rs/azure_identity/latest/azure_identity/) crate's
`DefaultAzureCredential`, which picks up your existing `az login` session.
`az` must be on your `PATH`.

## License

MIT OR Apache-2.0.
