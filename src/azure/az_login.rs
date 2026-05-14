//! In-app `az login` shell-out.
//!
//! The TUI suspends the alternate screen / raw mode, this module spawns
//! `az login` with inherited stdio so the user sees the browser prompt or
//! device-code text directly, then returns once `az` exits. The caller is
//! responsible for resuming the TUI and clearing any cached tokens.

use std::io::ErrorKind;

use anyhow::{anyhow, Result};
use tokio::process::Command;

#[derive(Clone, Debug, Default)]
pub struct AzLoginOptions {
    /// `--tenant <id-or-domain>`. Useful when the user is a guest in the
    /// tenant whose subscriptions they want to see.
    pub tenant: Option<String>,
    /// `--use-device-code`. Required for headless / SSH sessions where
    /// `az login` can't open a browser.
    pub use_device_code: bool,
}

/// Spawn `az login` with inherited stdio and wait for it to exit.
///
/// Returns `Ok(())` on a clean exit, an error with the install hint when
/// `az` is not on PATH, and a generic non-zero-exit error otherwise.
pub async fn run(opts: AzLoginOptions) -> Result<()> {
    let mut cmd = Command::new("az");
    cmd.arg("login");
    if let Some(tenant) = opts.tenant.as_deref() {
        cmd.arg("--tenant").arg(tenant);
    }
    if opts.use_device_code {
        cmd.arg("--use-device-code");
    }
    // stdio is inherited by default for tokio::process::Command, which is what
    // we want here — the user needs to see and respond to az's prompts.

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            anyhow!(
                "`az` CLI not found in PATH. Install it from https://aka.ms/azcli, then try again."
            )
        } else {
            anyhow!("failed to launch `az login`: {e}")
        }
    })?;

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow!("failed to wait for `az login`: {e}"))?;

    if !status.success() {
        return Err(anyhow!(
            "`az login` exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string())
        ));
    }
    Ok(())
}
