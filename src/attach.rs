use crate::config::{Config, HostKind};
use crate::snapshot;
use crate::ssh;
use crate::tmux::{self, PaneTarget};
use anyhow::{Context, Result};
use std::env;
use std::path::Path;
use std::process::Command;

pub fn attach(config: &Config, id_or_target: &str, readonly: bool) -> Result<()> {
    let target = snapshot::target_for_action(config, id_or_target, "attach")?;
    attach_target(config, &target, readonly)
}

pub fn attach_target(config: &Config, target: &PaneTarget, readonly: bool) -> Result<()> {
    let host = config.host(&target.host)?;
    match host.kind {
        HostKind::Local => attach_local(target, readonly, host.tmux_socket()),
        HostKind::Ssh | HostKind::Docker => {
            let command = attach_command(target, readonly, host.tmux_socket());
            ssh::run_interactive(host, &command, config.poll.ssh_timeout)
                .with_context(|| format!("failed to run remote tmux attach for `{target}`"))
        }
    }
}

fn attach_local(target: &PaneTarget, readonly: bool, socket: Option<&str>) -> Result<()> {
    let mut command = Command::new("tmux");
    if let Some(socket) = socket {
        command
            .arg("-S")
            .arg(crate::config::expand_home_path(Path::new(socket)));
        command.env_remove("TMUX");
    }
    if should_switch_client(socket, inside_tmux()) {
        // Inside an existing tmux client, `switch-client -t <pane-target>`
        // is the documented way to change session/window/pane atomically —
        // tmux special-cases a target containing `:`, `.`, or `%`.
        //
        // `switch-client -r` toggles the client's read-only flag, so calling
        // it twice would flip back to writable. Use `-f read-only,ignore-size`
        // to set the flags deterministically instead.
        command.arg("switch-client");
        if readonly {
            command.arg("-f").arg("read-only,ignore-size");
        }
        command.arg("-t").arg(switch_client_target(target));
    } else {
        command.arg("attach-session");
        if readonly {
            command.arg("-r");
        }
        command.arg("-t").arg(&target.session);
        add_select_args(&mut command, target);
    }

    let status = command
        .status()
        .with_context(|| format!("failed to start tmux attach for `{target}`"))?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("tmux attach for `{target}` exited with status {status}")
    }
}

fn inside_tmux() -> bool {
    env::var_os("TMUX").is_some()
}

fn should_switch_client(socket: Option<&str>, inside_tmux: bool) -> bool {
    socket.is_none() && inside_tmux
}

/// Build a single-target string for `switch-client -t` that selects
/// session, window, and pane. Prefers the global `%pane_id` when known,
/// otherwise falls back to `session:window.pane`.
fn switch_client_target(target: &PaneTarget) -> String {
    target
        .pane_id
        .clone()
        .unwrap_or_else(|| target.tmux_target())
}

fn add_select_args(command: &mut Command, target: &PaneTarget) {
    // `select-pane -t %id` only activates the pane *inside its window*; it
    // does not switch the client's current window. Always issue an explicit
    // `select-window` first. `select-window -t` takes a target-window, not a
    // target-pane, so we always use the window index — the prior
    // `attach-session -t <session>` already set the client's session.
    command
        .arg(";")
        .arg("select-window")
        .arg("-t")
        .arg(&target.window);
    command
        .arg(";")
        .arg("select-pane")
        .arg("-t")
        .arg(select_pane_target(target));
}

fn attach_command(target: &PaneTarget, readonly: bool, socket: Option<&str>) -> String {
    let mut parts = vec![tmux::tmux_command(socket), "attach-session".to_string()];
    if readonly {
        parts.push("-r".to_string());
    }
    parts.push("-t".to_string());
    parts.push(tmux::shell_quote(&target.session));
    add_select_parts(&mut parts, target);
    parts.join(" ")
}

fn add_select_parts(parts: &mut Vec<String>, target: &PaneTarget) {
    // Same correctness concern as `add_select_args`. `select-window` takes a
    // target-window, so always use the window index.
    parts.push("\\; select-window -t".to_string());
    parts.push(tmux::shell_quote(&target.window));
    parts.push("\\; select-pane -t".to_string());
    parts.push(tmux::shell_quote(select_pane_target(target)));
}

/// Prefer the globally unique `%pane_id` for `select-pane` when known,
/// falling back to the per-window pane index.
fn select_pane_target(target: &PaneTarget) -> &str {
    target.pane_id.as_deref().unwrap_or(&target.pane)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_target(pane_id: Option<&str>) -> PaneTarget {
        PaneTarget {
            host: "h".to_string(),
            session: "s".to_string(),
            window: "1".to_string(),
            pane: "2".to_string(),
            pane_id: pane_id.map(str::to_string),
        }
    }

    #[test]
    fn attach_command_with_pane_id_uses_window_index_and_pane_id() {
        let cmd = attach_command(&pane_target(Some("%42")), false, None);
        assert!(cmd.contains("attach-session"));
        assert!(cmd.contains("-t 's'"));
        // select-window uses a window target (index), not the pane_id.
        assert!(
            cmd.contains("select-window -t '1'"),
            "expected window index target: {cmd}"
        );
        // select-pane uses the globally unique pane_id.
        let window_idx = cmd.find("select-window -t '1'").expect("window select");
        let pane_idx = cmd.find("select-pane -t '%42'").expect("pane select");
        assert!(
            window_idx < pane_idx,
            "select-window must precede select-pane: {cmd}"
        );
    }

    #[test]
    fn attach_command_without_pane_id_uses_window_and_pane_indices() {
        let cmd = attach_command(&pane_target(None), true, None);
        assert!(cmd.contains("attach-session -r"));
        assert!(cmd.contains("select-window -t '1'"));
        assert!(cmd.contains("select-pane -t '2'"));
    }

    #[test]
    fn attach_command_includes_custom_socket() {
        let cmd = attach_command(&pane_target(Some("%42")), true, Some("/tmp/tmux.sock"));
        assert!(cmd.starts_with("tmux -S '/tmp/tmux.sock' attach-session -r"));
        assert!(cmd.contains("select-pane -t '%42'"));
    }

    #[test]
    fn configured_socket_does_not_switch_existing_client() {
        assert!(should_switch_client(None, true));
        assert!(!should_switch_client(Some("/tmp/tmux.sock"), true));
        assert!(!should_switch_client(Some("/tmp/tmux.sock"), false));
    }

    #[test]
    fn switch_client_target_prefers_pane_id() {
        assert_eq!(switch_client_target(&pane_target(Some("%9"))), "%9");
        assert_eq!(switch_client_target(&pane_target(None)), "s:1.2");
    }
}
