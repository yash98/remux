use crate::config::{Config, HostKind};
use crate::exit::ExitFailure;
use crate::host;
use crate::launch_template::{LaunchTemplatePreset, launch_session_name, launch_template_preset};
use crate::snapshot::{self, MatchStatus};
use crate::tmux::{self, PaneTarget};
use anyhow::{Context, Result};
use std::io::{self, IsTerminal, Write};

enum KillTarget {
    Session(SessionTarget),
    Pane(PaneTarget),
}

enum SendTarget {
    Session(SessionTarget),
    Pane(PaneTarget),
}

struct SessionTarget {
    host: String,
    session: String,
}

#[derive(Clone, Copy, Debug)]
pub struct LaunchTemplateStartOptions<'a> {
    pub cwd: Option<&'a str>,
    pub window_name: Option<&'a str>,
    pub send_startup_keys: bool,
    pub verbose: bool,
}

pub fn new_session(
    config: &Config,
    host_id: &str,
    session_name: &str,
    cwd: Option<&str>,
    window_name: Option<&str>,
    verbose: bool,
) -> Result<()> {
    config.resolve_host(host_id)?;
    let snapshot = snapshot::snapshot_host(config, host_id)?;
    if snapshot.sessions.iter().any(|row| {
        row.raw_target.is_some()
            && row.host == host_id
            && row.tmux.session == session_name
            && !matches!(
                row.match_status,
                MatchStatus::Missing | MatchStatus::Unreachable
            )
    }) {
        return Err(ExitFailure::new(
            2,
            format!("session `{host_id}/{session_name}` already exists"),
        )
        .into());
    }

    let host = config.resolve_host(host_id)?;
    let command = tmux::new_session_command(session_name, cwd, window_name, host.tmux_socket());
    run_lifecycle_command(config, host_id, &command, verbose)
        .with_context(|| format!("failed to create session `{host_id}/{session_name}`"))
}

pub fn kill(config: &Config, target: &str, yes: bool, verbose: bool) -> Result<()> {
    let target = resolve_kill_target(config, target)?;
    confirm_kill(&target, yes)?;
    let (host_id, command) = target.command(config)?;
    run_lifecycle_command(config, host_id, &command, verbose)
}

pub fn send_keys(
    config: &Config,
    target: &str,
    keys: &str,
    enter: bool,
    verbose: bool,
) -> Result<()> {
    let target = resolve_send_target(config, target)?;
    let (host_id, command) = target.command(config, keys, enter)?;
    run_lifecycle_command(config, host_id, &command, verbose)
}

pub fn start_launch_template(
    config: &Config,
    host_id: &str,
    preset_id: &str,
    session_name_suffix: &str,
    options: LaunchTemplateStartOptions<'_>,
) -> Result<String> {
    let preset = launch_template_preset(config, preset_id)?;
    start_launch_template_preset(config, host_id, &preset, session_name_suffix, options)
}

pub fn start_launch_template_preset(
    config: &Config,
    host_id: &str,
    preset: &LaunchTemplatePreset,
    session_name_suffix: &str,
    options: LaunchTemplateStartOptions<'_>,
) -> Result<String> {
    let session_name = launch_session_name(preset, session_name_suffix)?;
    let window_name = options.window_name.or(preset.window_name.as_deref());
    new_session(
        config,
        host_id,
        &session_name,
        options.cwd,
        window_name,
        options.verbose,
    )?;
    if options.send_startup_keys {
        send_keys_to_session(
            config,
            host_id,
            &session_name,
            &preset.command,
            true,
            options.verbose,
        )
        .with_context(|| {
            format!(
                "failed to send launch template `{}` startup keys to `{host_id}/{session_name}`",
                preset.id
            )
        })?;
    }
    Ok(session_name)
}

pub fn rename_session(
    config: &Config,
    host_id: &str,
    old_name: &str,
    new_name: &str,
    verbose: bool,
) -> Result<()> {
    config.resolve_host(host_id)?;
    let snapshot = snapshot::snapshot_host(config, host_id)?;
    let old_exists = snapshot
        .sessions
        .iter()
        .any(|row| row.raw_target.is_some() && row.tmux.session == old_name);
    if !old_exists {
        return Err(
            ExitFailure::new(3, format!("session `{host_id}/{old_name}` was not found")).into(),
        );
    }
    let new_exists = snapshot
        .sessions
        .iter()
        .any(|row| row.raw_target.is_some() && row.tmux.session == new_name);
    if new_exists {
        return Err(
            ExitFailure::new(2, format!("session `{host_id}/{new_name}` already exists")).into(),
        );
    }

    let host = config.resolve_host(host_id)?;
    run_lifecycle_command(
        config,
        host_id,
        &tmux::rename_session_command(old_name, new_name, host.tmux_socket()),
        verbose,
    )
    .with_context(|| format!("failed to rename `{host_id}/{old_name}` to `{new_name}`"))
}

pub fn new_pane(config: &Config, host_id: &str, session: &str, verbose: bool) -> Result<()> {
    config.resolve_host(host_id)?;
    let snapshot = snapshot::snapshot_host(config, host_id)?;
    let session_exists = snapshot
        .sessions
        .iter()
        .any(|row| row.raw_target.is_some() && row.tmux.session == session);
    if !session_exists {
        return Err(
            ExitFailure::new(3, format!("session `{host_id}/{session}` was not found")).into(),
        );
    }

    let host = config.resolve_host(host_id)?;
    run_lifecycle_command(
        config,
        host_id,
        &tmux::split_window_command(session, host.tmux_socket()),
        verbose,
    )
    .with_context(|| format!("failed to spawn pane in `{host_id}/{session}`"))
}

fn send_keys_to_session(
    config: &Config,
    host_id: &str,
    session: &str,
    keys: &str,
    enter: bool,
    verbose: bool,
) -> Result<()> {
    let host = config.resolve_host(host_id)?;
    let command = tmux::send_keys_command(session, keys, enter, host.tmux_socket());
    run_lifecycle_command(config, host_id, &command, verbose)
}

fn resolve_kill_target(config: &Config, target: &str) -> Result<KillTarget> {
    if config.find_watch(target).is_some() {
        return snapshot::target_for_action(config, target, "kill")
            .map(KillTarget::Pane)
            .map_err(|err| {
                ExitFailure::new(3, format!("cannot kill watch `{target}`: {err:#}")).into()
            });
    }

    if let Ok(pane) = PaneTarget::parse(target) {
        let resolved = resolve_live_pane(config, pane)?;
        return Ok(KillTarget::Pane(resolved));
    }

    if let Some((host, session)) = parse_session_target(target) {
        let session = resolve_live_session(config, host, session)?;
        return Ok(KillTarget::Session(session));
    }

    Err(ExitFailure::new(
        3,
        format!("target `{target}` was not found or is ambiguous"),
    )
    .into())
}

fn resolve_send_target(config: &Config, target: &str) -> Result<SendTarget> {
    if config.find_watch(target).is_some() {
        return snapshot::target_for_action(config, target, "send keys")
            .map(SendTarget::Pane)
            .map_err(|err| {
                ExitFailure::new(3, format!("cannot send keys to watch `{target}`: {err:#}")).into()
            });
    }

    if let Ok(pane) = PaneTarget::parse(target) {
        let resolved = resolve_live_pane(config, pane)?;
        return Ok(SendTarget::Pane(resolved));
    }

    if let Some((host, session)) = parse_session_target(target) {
        return Ok(SendTarget::Session(resolve_live_session(
            config, host, session,
        )?));
    }

    Err(ExitFailure::new(
        3,
        format!("target `{target}` was not found or is ambiguous"),
    )
    .into())
}

fn resolve_live_pane(config: &Config, target: PaneTarget) -> Result<PaneTarget> {
    let snapshot = snapshot::snapshot_host(config, &target.host)?;
    let target_string = target.to_string();
    let row = snapshot
        .sessions
        .into_iter()
        .find(|row| row.raw_target.as_deref() == Some(target_string.as_str()))
        .ok_or_else(|| {
            ExitFailure::new(
                3,
                format!("pane `{target_string}` was not found in latest snapshot"),
            )
        })?;
    Ok(PaneTarget {
        host: row.host,
        session: row.tmux.session,
        window: row.tmux.window.ok_or_else(|| {
            ExitFailure::new(3, format!("pane `{target_string}` has no resolved window"))
        })?,
        pane: row.tmux.pane.ok_or_else(|| {
            ExitFailure::new(3, format!("pane `{target_string}` has no resolved pane"))
        })?,
        pane_id: row.tmux.pane_id,
    })
}

fn resolve_live_session(config: &Config, host: &str, session: &str) -> Result<SessionTarget> {
    let snapshot = snapshot::snapshot_host(config, host).map_err(|err| {
        ExitFailure::new(
            3,
            format!("session `{host}/{session}` could not be resolved: {err:#}"),
        )
    })?;
    let found = snapshot
        .sessions
        .iter()
        .any(|row| row.raw_target.is_some() && row.tmux.session == session);
    if found {
        Ok(SessionTarget {
            host: host.to_string(),
            session: session.to_string(),
        })
    } else {
        Err(ExitFailure::new(3, format!("session `{host}/{session}` was not found")).into())
    }
}

fn parse_session_target(target: &str) -> Option<(&str, &str)> {
    let (host, session) = target.split_once('/')?;
    if host.is_empty() || session.is_empty() || session.contains(':') {
        return None;
    }
    Some((host, session))
}

fn confirm_kill(target: &KillTarget, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        return Err(ExitFailure::new(2, "refusing to kill without --yes on non-TTY stdin").into());
    }

    eprint!("kill {}? [y/N] ", target.display());
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y") {
        Ok(())
    } else {
        Err(ExitFailure::new(2, "kill cancelled").into())
    }
}

fn run_lifecycle_command(
    config: &Config,
    host_id: &str,
    command: &str,
    verbose: bool,
) -> Result<()> {
    let host_config = config.resolve_host(host_id)?;
    if verbose {
        match host_config.kind {
            HostKind::Local => eprintln!("{command}"),
            HostKind::Ssh | HostKind::Docker => eprintln!("ssh {} -- {command}", host_id),
        }
    }
    host::run(config, &host_config, command).map(|_| ())
}

impl KillTarget {
    fn command(&self, config: &Config) -> Result<(&str, String)> {
        match self {
            KillTarget::Session(target) => {
                let host_config = config.resolve_host(&target.host)?;
                Ok((
                    &target.host,
                    tmux::kill_session_command(&target.session, host_config.tmux_socket()),
                ))
            }
            KillTarget::Pane(target) => {
                let host_config = config.resolve_host(&target.host)?;
                Ok((
                    &target.host,
                    tmux::kill_pane_command(target, host_config.tmux_socket()),
                ))
            }
        }
    }

    fn display(&self) -> String {
        match self {
            KillTarget::Session(target) => format!("{}/{}", target.host, target.session),
            KillTarget::Pane(target) => target.to_string(),
        }
    }
}

impl SendTarget {
    fn command(&self, config: &Config, keys: &str, enter: bool) -> Result<(&str, String)> {
        match self {
            SendTarget::Session(target) => {
                let host_config = config.resolve_host(&target.host)?;
                Ok((
                    &target.host,
                    tmux::send_keys_command(
                        &target.session,
                        keys,
                        enter,
                        host_config.tmux_socket(),
                    ),
                ))
            }
            SendTarget::Pane(target) => {
                let host_config = config.resolve_host(&target.host)?;
                Ok((
                    &target.host,
                    tmux::send_keys_command(
                        &target.tmux_target(),
                        keys,
                        enter,
                        host_config.tmux_socket(),
                    ),
                ))
            }
        }
    }
}
