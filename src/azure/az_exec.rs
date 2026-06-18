//! In-app `az containerapp exec` shell-out — k9s-style `s` to drop into a
//! running container.
//!
//! Mirrors [`crate::azure::az_login`]: the TUI suspends the alternate screen /
//! raw mode, this module spawns `az containerapp exec` with inherited stdio so
//! the user gets a fully interactive shell (az handles the exec websocket /
//! PTY), then returns once the shell exits. The caller resumes the TUI.
//!
//! We shell out rather than open the exec websocket ourselves because `az` is
//! already the credential backend here (`DefaultAzureCredential` → `az account
//! get-access-token`) and it implements the terminal resize / stdin streaming a
//! native client would otherwise have to reproduce.

use std::io::ErrorKind;

use anyhow::{anyhow, Result};
use tokio::process::Command;

/// Target + command for one `az containerapp exec` invocation. The optional
/// fields are only passed through when known; anything omitted lets `az` pick
/// its default (latest revision, an arbitrary replica, the first container).
#[derive(Clone, Debug, Default)]
pub struct AzExecOptions {
    pub name: String,
    pub resource_group: String,
    /// `--subscription`. The app's own subscription, so the exec targets the
    /// right one regardless of `az`'s currently-active subscription.
    pub subscription: Option<String>,
    /// `--revision`. Required for `--replica` to resolve, so the arg builder
    /// only emits `--replica`/`--container` when this is also set.
    pub revision: Option<String>,
    pub replica: Option<String>,
    pub container: Option<String>,
    /// Shell/command to launch inside the container. Falls back to `/bin/sh`
    /// when empty — the most broadly available shell across base images.
    pub command: String,
}

/// Build the `az` argument vector (everything after the `az` program name).
/// Pulled out so tests can assert the exact flags without spawning a process.
/// `--replica`/`--container` are gated behind `--revision` because the CLI
/// can't resolve a replica without knowing its revision.
pub fn build_args(opts: &AzExecOptions) -> Vec<String> {
    let mut args = vec![
        "containerapp".to_string(),
        "exec".to_string(),
        "--name".to_string(),
        opts.name.clone(),
        "--resource-group".to_string(),
        opts.resource_group.clone(),
    ];
    if let Some(sub) = opts.subscription.as_deref() {
        args.push("--subscription".to_string());
        args.push(sub.to_string());
    }
    if let Some(rev) = opts.revision.as_deref() {
        args.push("--revision".to_string());
        args.push(rev.to_string());
        if let Some(replica) = opts.replica.as_deref() {
            args.push("--replica".to_string());
            args.push(replica.to_string());
            if let Some(container) = opts.container.as_deref() {
                args.push("--container".to_string());
                args.push(container.to_string());
            }
        }
    }
    let command = if opts.command.trim().is_empty() {
        "/bin/sh"
    } else {
        opts.command.trim()
    };
    args.push("--command".to_string());
    args.push(command.to_string());
    args
}

/// RAII guard that sets `SIGINT`/`SIGQUIT`/`SIGTSTP` to ignored for its lifetime
/// and restores the previous dispositions on drop. Used to shield the parent
/// `azpect` process while a shell-out child owns the terminal. No-op on
/// non-unix.
struct IgnoreTerminalSignals {
    #[cfg(unix)]
    prev: [(libc::c_int, libc::sighandler_t); 3],
}

impl IgnoreTerminalSignals {
    #[cfg(unix)]
    fn install() -> Self {
        let signals = [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP];
        let mut prev = [(0, 0 as libc::sighandler_t); 3];
        for (slot, sig) in prev.iter_mut().zip(signals) {
            // SAFETY: `signal` with SIG_IGN is async-signal-safe and the only
            // global state we touch; the previous handler is captured for
            // restore on drop.
            let old = unsafe { libc::signal(sig, libc::SIG_IGN) };
            *slot = (sig, old);
        }
        Self { prev }
    }

    #[cfg(not(unix))]
    fn install() -> Self {
        Self {}
    }
}

#[cfg(unix)]
impl Drop for IgnoreTerminalSignals {
    fn drop(&mut self) {
        for (sig, old) in self.prev {
            // SAFETY: restoring the disposition captured in `install`.
            unsafe {
                libc::signal(sig, old);
            }
        }
    }
}

/// Child-side (`pre_exec`) reset of the terminal job-control signals back to
/// their default disposition. `exec` preserves `SIG_IGN`, so without this the
/// child would inherit the parent's ignore and `az` / the remote shell couldn't
/// be interrupted at all. Only calls the async-signal-safe `signal`.
#[cfg(unix)]
fn reset_child_signals() -> std::io::Result<()> {
    for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP] {
        // SAFETY: async-signal-safe; runs in the forked child before exec.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
        }
    }
    Ok(())
}

/// Spawn `az containerapp exec` with inherited stdio and wait for the shell to
/// exit. Returns the install hint when `az` is missing, and a non-zero-exit
/// error otherwise (the user has already seen az's own stderr inline).
///
/// While the child runs, the parent ignores the terminal job-control signals
/// (`SIGINT`/`SIGQUIT`/`SIGTSTP`). With the TUI suspended the terminal is in
/// cooked mode, so a Ctrl-C reaches the whole foreground process group — without
/// this guard it would terminate `azpect` itself instead of just interrupting
/// the remote shell, dropping the user back to their parent shell rather than
/// the TUI. The child resets those signals to default (see [`reset_child_signals`])
/// so `az` and the remote shell behave normally.
pub async fn run(opts: AzExecOptions) -> Result<()> {
    let mut cmd = Command::new("az");
    cmd.args(build_args(&opts));
    // stdio is inherited by default for tokio::process::Command — exactly what
    // an interactive shell needs. We deliberately leave `az` in azpect's process
    // group and let it manage the terminal itself: it sets raw mode, reads
    // stdin, and forwards keystrokes (including Ctrl-C as a byte) over its exec
    // websocket. Handing it a separate foreground process group (job control)
    // fought that management and made its stdin reads fail intermittently
    // (EIO), dropping input — so we don't.
    #[cfg(unix)]
    unsafe {
        // SAFETY: `reset_child_signals` only calls async-signal-safe `signal`.
        // Resets the job-control signals the child inherits as SIG_IGN from the
        // parent guard below, so `az` and the remote shell can be interrupted.
        cmd.pre_exec(reset_child_signals);
    }

    // Keep terminal signals from killing azpect while the child owns the
    // terminal. Because `az` shares our process group, a Ctrl-C in any cooked
    // window still reaches `az` directly (it resets to default above) and
    // interrupts the remote command — this guard only spares azpect itself.
    let _signals = IgnoreTerminalSignals::install();

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            anyhow!(
                "`az` CLI not found in PATH. Install it from https://aka.ms/azcli, then try again."
            )
        } else {
            anyhow!("failed to launch `az containerapp exec`: {e}")
        }
    })?;

    let status = child
        .wait()
        .await
        .map_err(|e| anyhow!("failed to wait for `az containerapp exec`: {e}"))?;

    if !status.success() {
        return Err(anyhow!(
            "`az containerapp exec` exited with status {}",
            status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string())
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_minimal_args_with_default_shell() {
        let opts = AzExecOptions {
            name: "ca-app".into(),
            resource_group: "rg".into(),
            command: String::new(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert_eq!(
            args,
            vec![
                "containerapp",
                "exec",
                "--name",
                "ca-app",
                "--resource-group",
                "rg",
                "--command",
                "/bin/sh",
            ]
        );
    }

    #[test]
    fn includes_full_target_when_revision_known() {
        let opts = AzExecOptions {
            name: "ca-app".into(),
            resource_group: "rg".into(),
            subscription: Some("sub-1".into()),
            revision: Some("ca-app--0000004".into()),
            replica: Some("ca-app--0000004-abc-xyz".into()),
            container: Some("maintenance".into()),
            command: "/bin/bash".into(),
        };
        let args = build_args(&opts);
        assert!(args.windows(2).any(|w| w == ["--subscription", "sub-1"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--revision", "ca-app--0000004"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--replica", "ca-app--0000004-abc-xyz"]));
        assert!(args.windows(2).any(|w| w == ["--container", "maintenance"]));
        assert!(args.windows(2).any(|w| w == ["--command", "/bin/bash"]));
    }

    #[test]
    fn omits_replica_and_container_without_revision() {
        // A replica can't be resolved without its revision, so the builder must
        // drop both rather than emit an unusable `--replica`.
        let opts = AzExecOptions {
            name: "ca-app".into(),
            resource_group: "rg".into(),
            revision: None,
            replica: Some("ca-app--0000004-abc-xyz".into()),
            container: Some("maintenance".into()),
            command: "/bin/sh".into(),
            ..Default::default()
        };
        let args = build_args(&opts);
        assert!(!args.iter().any(|a| a == "--replica"));
        assert!(!args.iter().any(|a| a == "--container"));
    }
}
