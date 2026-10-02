use crate::config::HostConfig;
use crate::exec;
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const MAX_PROXYJUMP_EXPANSION_DEPTH: usize = 8;

pub fn run(
    host: &HostConfig,
    remote_command: &str,
    ssh_timeout: Duration,
    command_timeout: Duration,
) -> Result<String> {
    let mut command = base_command(host, ssh_timeout, false)?;
    append_remote_command(&mut command, host, remote_command)?;
    let output = exec::output(
        &mut command,
        command_timeout,
        format!("ssh command for host `{}`", host.id),
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("ssh exited with status {}", output.status)
        } else {
            stderr
        };
        return Err(anyhow!(
            "failed to run remote command on host `{}`: {message}\nhint: verify `ssh {}` works non-interactively",
            host.id,
            host.ssh()?
                .target()
                .unwrap_or_else(|| "<target>".to_string())
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn run_interactive(
    host: &HostConfig,
    remote_command: &str,
    default_timeout: Duration,
) -> Result<()> {
    let mut command = base_command(host, default_timeout, true)?;
    append_remote_command(&mut command, host, remote_command)?;
    let status = command
        .status()
        .with_context(|| format!("failed to start interactive ssh for host `{}`", host.id))?;

    if status.success() {
        Ok(())
    } else if status.code() == Some(255) {
        Err(anyhow!(
            "ssh connection failed for host `{}` with status {status}",
            host.id
        ))
    } else {
        Err(anyhow!(
            "remote interactive command on host `{}` exited with status {status}",
            host.id
        ))
    }
}

fn base_command(host: &HostConfig, default_timeout: Duration, tty: bool) -> Result<Command> {
    let ssh = host.ssh()?;
    let target = ssh
        .target()
        .ok_or_else(|| anyhow!("host `{}` is missing ssh target", host.id))?;

    let mut command = Command::new("ssh");
    if let Some(config_file) = &ssh.config_file {
        command.arg("-F").arg(config_file);
    }
    let mut options = ssh.ssh_options(default_timeout);
    if tty {
        apply_interactive_options(&mut options, ssh.config_file.as_deref(), ssh.port, &target);
    }
    for (key, value) in options {
        command.arg("-o").arg(format!("{key}={value}"));
    }
    if let Some(port) = ssh.port {
        command.arg("-p").arg(port.to_string());
    }
    if tty {
        command.arg("-t");
    }
    command.arg(target);
    if let Some(container) = host.container() {
        command.arg("docker").arg("exec").arg("-i").arg(container);
    }
    Ok(command)
}

fn apply_interactive_options(
    options: &mut BTreeMap<String, String>,
    config_file: Option<&Path>,
    port: Option<u16>,
    target: &str,
) {
    // A long-lived interactive attach should not reuse a stale multiplexed
    // connection after sleep/wake. Polling may still opt into muxing for speed,
    // and explicit remux ssh.options can override these defaults.
    let control_master = option_value(options, "ControlMaster").map(str::to_string);
    if control_master.is_none() {
        options.insert("ControlMaster".to_string(), "no".to_string());
    }
    if control_master
        .as_deref()
        .is_none_or(control_master_disables_muxing)
        && option_value(options, "ControlPath").is_none()
    {
        options.insert("ControlPath".to_string(), "none".to_string());
    }
    disable_proxyjump_muxing(options, config_file, port, target);
}

fn control_master_disables_muxing(value: &str) -> bool {
    value.eq_ignore_ascii_case("no") || value.eq_ignore_ascii_case("false")
}

fn option_value<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|(option_key, _)| option_key.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

fn disable_proxyjump_muxing(
    options: &mut BTreeMap<String, String>,
    config_file: Option<&Path>,
    port: Option<u16>,
    target: &str,
) {
    if option_value(options, "ProxyCommand").is_some() {
        return;
    }
    let explicit_proxy_jump = option_value(options, "ProxyJump").map(str::to_string);
    let port = port.map(|port| port.to_string());
    let proxy_jump = explicit_proxy_jump.or_else(|| {
        resolved_proxy_jump_from_ssh_config(config_file, port.as_deref(), target, options)
    });
    let Some(proxy_jump) = proxy_jump else {
        return;
    };
    let Some(proxy_command) = proxyjump_proxy_command(&proxy_jump, options, config_file) else {
        return;
    };
    if let Some(key) = options
        .keys()
        .find(|key| key.eq_ignore_ascii_case("ProxyJump"))
        .cloned()
    {
        options.remove(&key);
    }
    options.insert("ProxyCommand".to_string(), proxy_command);
}

fn proxyjump_proxy_command(
    proxy_jump: &str,
    options: &BTreeMap<String, String>,
    config_file: Option<&Path>,
) -> Option<String> {
    if proxy_jump.eq_ignore_ascii_case("none") {
        return None;
    }
    let jumps = parse_proxyjump_chain(proxy_jump)?;
    let jumps = expand_proxyjump_aliases(jumps, options, config_file, 0)?;
    proxy_command_for_hop(&jumps, jumps.len().checked_sub(1)?, options, config_file, 0)
}

fn parse_proxyjump_chain(proxy_jump: &str) -> Option<Vec<ProxyJumpHop>> {
    proxy_jump
        .split(',')
        .map(str::trim)
        .filter(|jump| !jump.is_empty())
        .map(parse_proxyjump_hop)
        .collect::<Option<_>>()
}

fn expand_proxyjump_aliases(
    jumps: Vec<ProxyJumpHop>,
    options: &BTreeMap<String, String>,
    config_file: Option<&Path>,
    depth: usize,
) -> Option<Vec<ProxyJumpHop>> {
    if depth >= MAX_PROXYJUMP_EXPANSION_DEPTH {
        return None;
    }
    let mut expanded = Vec::new();
    for hop in jumps {
        if let Some(proxy_jump) = resolved_proxy_jump_from_ssh_config(
            config_file,
            hop.port.as_deref(),
            &hop.destination,
            options,
        ) {
            let nested = parse_proxyjump_chain(&proxy_jump)?;
            expanded.extend(expand_proxyjump_aliases(
                nested,
                options,
                config_file,
                depth + 1,
            )?);
        }
        expanded.push(hop);
    }
    Some(expanded)
}

#[derive(Debug, PartialEq, Eq)]
struct ProxyJumpHop {
    destination: String,
    port: Option<String>,
}

fn parse_proxyjump_hop(raw: &str) -> Option<ProxyJumpHop> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(uri) = raw.strip_prefix("ssh://") {
        let uri = uri.strip_suffix('/').unwrap_or(uri);
        return parse_proxyjump_hop(uri);
    }
    let (user_prefix, host_port) = match raw.rsplit_once('@') {
        Some((_, "")) => return None,
        Some((user, host_port)) => (format!("{user}@"), host_port),
        None => (String::new(), raw),
    };

    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = rest[..close].to_string();
        let suffix = &rest[close + 1..];
        if !suffix.is_empty() && !suffix.starts_with(':') {
            return None;
        }
        let port = suffix
            .strip_prefix(':')
            .filter(|port| !port.is_empty())
            .map(str::to_string);
        (host, port)
    } else if let Some((host, port)) = host_port.rsplit_once(':') {
        if host.contains(':') {
            (host_port.to_string(), None)
        } else {
            let port = (!port.is_empty()).then(|| port.to_string());
            (host.to_string(), port)
        }
    } else {
        (host_port.to_string(), None)
    };

    if host.is_empty() {
        return None;
    }
    Some(ProxyJumpHop {
        destination: format!("{user_prefix}{host}"),
        port,
    })
}

fn proxy_command_for_hop(
    jumps: &[ProxyJumpHop],
    index: usize,
    options: &BTreeMap<String, String>,
    config_file: Option<&Path>,
    escape_level: usize,
) -> Option<String> {
    let hop = jumps.get(index)?;
    let mut parts = vec!["ssh".to_string()];
    if let Some(config_file) = config_file {
        parts.push("-F".to_string());
        parts.push(shell_single_quote(&config_file.to_string_lossy()));
    }
    for key in [
        "ConnectTimeout",
        "ServerAliveInterval",
        "ServerAliveCountMax",
        "ControlMaster",
        "ControlPath",
    ] {
        if let Some(value) = option_value(options, key) {
            parts.push("-o".to_string());
            parts.push(shell_single_quote(&format!(
                "{key}={}",
                escape_percent_tokens(value, escape_level + 1)
            )));
        }
    }
    if index > 0 {
        let nested =
            proxy_command_for_hop(jumps, index - 1, options, config_file, escape_level + 1)?;
        parts.push("-o".to_string());
        parts.push(shell_single_quote(&format!("ProxyCommand={nested}")));
    }
    parts.push("-W".to_string());
    parts.push(shell_single_quote(&stdio_forward_target(escape_level)));
    if let Some(port) = &hop.port {
        parts.push("-p".to_string());
        parts.push(shell_single_quote(port));
    }
    parts.push(shell_single_quote(&escape_percent_tokens(
        &hop.destination,
        escape_level + 1,
    )));
    Some(parts.join(" "))
}

fn stdio_forward_target(escape_level: usize) -> String {
    let percent = percent_tokens(escape_level);
    format!("[{percent}h]:{percent}p")
}

fn escape_percent_tokens(value: &str, escape_level: usize) -> String {
    value.replace('%', &percent_tokens(escape_level))
}

fn percent_tokens(escape_level: usize) -> String {
    "%".repeat(1usize.checked_shl(escape_level as u32).unwrap_or(1))
}

fn resolved_proxy_jump_from_ssh_config(
    config_file: Option<&Path>,
    port: Option<&str>,
    target: &str,
    options: &BTreeMap<String, String>,
) -> Option<String> {
    let mut command = Command::new("ssh");
    command.arg("-G");
    if let Some(config_file) = config_file {
        command.arg("-F").arg(config_file);
    }
    for (key, value) in options {
        if key.eq_ignore_ascii_case("ProxyCommand") || key.eq_ignore_ascii_case("ProxyJump") {
            continue;
        }
        command.arg("-o").arg(format!("{key}={value}"));
    }
    if let Some(port) = port {
        command.arg("-p").arg(port);
    }
    command.arg(target);

    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_ssh_g_proxy(&String::from_utf8_lossy(&output.stdout))
}

fn parse_ssh_g_proxy(raw: &str) -> Option<String> {
    let mut proxy_jump = None;
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if value.eq_ignore_ascii_case("none") {
            continue;
        }
        if key.eq_ignore_ascii_case("proxyjump") {
            proxy_jump = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("proxycommand") {
            return None;
        }
    }
    proxy_jump
}

fn append_remote_command(
    command: &mut Command,
    host: &HostConfig,
    remote_command: &str,
) -> Result<()> {
    let ssh = host.ssh()?;
    match &ssh.remote_shell {
        Some(shell) if !shell.is_empty() => {
            // SSH concatenates all post-target args with spaces on the remote
            // side, so the shell invocation must be a single ssh argument with
            // the command string properly quoted.
            let mut joined = String::new();
            for (i, part) in shell.iter().enumerate() {
                if i > 0 {
                    joined.push(' ');
                }
                joined.push_str(&shell_single_quote(part));
            }
            joined.push(' ');
            joined.push_str(&shell_single_quote(remote_command));
            command.arg(joined);
        }
        _ => {
            command.arg(remote_command);
        }
    }
    Ok(())
}

/// Wrap `input` in POSIX single quotes, escaping any existing single quotes.
fn shell_single_quote(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('\'');
    for ch in input.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use crate::config::{HostConfig, HostKind, SshConfig};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn docker_host_wraps_remote_command_in_docker_exec() {
        let mut host = ssh_host(BTreeMap::new());
        host.kind = HostKind::Docker;
        host.container = Some("yash-f5-tts".to_string());

        let command = super::base_command(&host, Duration::from_secs(5), false).unwrap();
        let args = command_args(&command);

        let pos = args
            .iter()
            .position(|arg| arg == "docker")
            .expect("docker prefix");
        assert_eq!(
            &args[pos..pos + 4],
            &["docker", "exec", "-i", "yash-f5-tts"]
        );
    }

    #[test]
    fn docker_host_interactive_adds_tty_for_ssh() {
        let mut host = ssh_host(BTreeMap::new());
        host.kind = HostKind::Docker;
        host.container = Some("yash-f5-tts".to_string());

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(args.contains(&"-t".to_string()));
        assert!(args.contains(&"docker".to_string()));
    }

    #[test]
    fn ssh_options_default_to_non_interactive_timeout() {
        let host = ssh_host(BTreeMap::new());

        let options = host.ssh.unwrap().ssh_options(Duration::from_secs(5));
        assert_eq!(options.get("BatchMode").unwrap(), "yes");
        assert_eq!(options.get("ConnectTimeout").unwrap(), "5");
        assert_eq!(options.get("ServerAliveInterval").unwrap(), "3");
        assert_eq!(options.get("ServerAliveCountMax").unwrap(), "2");
    }

    #[test]
    fn interactive_ssh_disables_multiplexing_by_default() {
        let host = ssh_host(BTreeMap::new());

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(has_ssh_option(&args, "ControlMaster=no"));
        assert!(has_ssh_option(&args, "ControlPath=none"));
    }

    #[test]
    fn non_interactive_ssh_does_not_disable_multiplexing_by_default() {
        let host = ssh_host(BTreeMap::new());

        let command = super::base_command(&host, Duration::from_secs(5), false).unwrap();
        let args = command_args(&command);

        assert!(!args.iter().any(|arg| arg.starts_with("ControlMaster=")));
        assert!(!args.iter().any(|arg| arg.starts_with("ControlPath=")));
    }

    #[test]
    fn explicit_mux_options_override_interactive_defaults_case_insensitively() {
        let mut options = BTreeMap::new();
        options.insert("controlmaster".to_string(), "auto".to_string());
        options.insert("Controlpath".to_string(), "/tmp/remux-ctl".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(has_ssh_option(&args, "controlmaster=auto"));
        assert!(has_ssh_option(&args, "Controlpath=/tmp/remux-ctl"));
        assert!(!has_ssh_option(&args, "ControlMaster=no"));
        assert!(!has_ssh_option(&args, "ControlPath=none"));
    }

    #[test]
    fn interactive_proxyjump_uses_proxycommand_with_mux_disabled() {
        let mut options = BTreeMap::new();
        options.insert("ProxyJump".to_string(), "jump".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(!args.iter().any(|arg| arg.starts_with("ProxyJump=")));
        let proxy_command = ssh_option(&args, "ProxyCommand").expect("ProxyCommand");
        assert!(proxy_command.contains("'ControlMaster=no'"));
        assert!(proxy_command.contains("'ControlPath=none'"));
        assert!(proxy_command.contains("-W '[%h]:%p' 'jump'"));
        assert!(!proxy_command.contains("BatchMode"));
    }

    #[test]
    fn explicit_control_master_auto_inherits_configured_control_path() {
        let mut options = BTreeMap::new();
        options.insert("ControlMaster".to_string(), "auto".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(has_ssh_option(&args, "ControlMaster=auto"));
        assert!(!has_ssh_option(&args, "ControlPath=none"));
    }

    #[test]
    fn explicit_control_master_no_still_disables_control_path() {
        let mut options = BTreeMap::new();
        options.insert("ControlMaster".to_string(), "no".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(has_ssh_option(&args, "ControlMaster=no"));
        assert!(has_ssh_option(&args, "ControlPath=none"));
    }

    #[test]
    fn proxyjump_inline_port_becomes_helper_port_argument() {
        let mut options = BTreeMap::new();
        options.insert("ProxyJump".to_string(), "bastion:2222".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);
        let proxy_command = ssh_option(&args, "ProxyCommand").expect("ProxyCommand");

        assert!(proxy_command.contains("-p '2222' 'bastion'"));
        assert!(!proxy_command.contains("'bastion:2222'"));
    }

    #[test]
    fn proxyjump_multiple_hops_uses_nested_proxycommands_not_nested_j() {
        let mut options = BTreeMap::new();
        options.insert("ProxyJump".to_string(), "jump1,jump2".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);
        let proxy_command = ssh_option(&args, "ProxyCommand").expect("ProxyCommand");

        assert!(proxy_command.contains("ProxyCommand="));
        assert!(proxy_command.contains("-W '[%h]:%p' 'jump2'"));
        assert!(proxy_command.contains("[%%h]:%%p"));
        assert!(proxy_command.contains("jump1"));
        assert!(!proxy_command.contains(" -J "));
    }

    #[test]
    fn proxyjump_expands_nested_alias_proxyjumps() {
        let config_file = temp_ssh_config(
            r#"
Host jump2
  ProxyJump jump1
"#,
        );
        let mut options = BTreeMap::new();
        options.insert("ProxyJump".to_string(), "jump2".to_string());
        let host = ssh_host_with_config(options, config_file.clone());

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);
        let proxy_command = ssh_option(&args, "ProxyCommand").expect("ProxyCommand");
        let _ = fs::remove_file(config_file);

        assert!(proxy_command.contains("ProxyCommand="));
        assert!(proxy_command.contains("-W '[%h]:%p' 'jump2'"));
        assert!(proxy_command.contains("[%%h]:%%p"));
        assert!(proxy_command.contains("jump1"));
        assert!(!proxy_command.contains(" -J "));
    }

    #[test]
    fn proxyjump_helper_escapes_controlpath_tokens() {
        let mut options = BTreeMap::new();
        options.insert("ProxyJump".to_string(), "jump".to_string());
        options.insert("ControlMaster".to_string(), "auto".to_string());
        options.insert("ControlPath".to_string(), "/tmp/remux-%r@%h:%p".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);
        let proxy_command = ssh_option(&args, "ProxyCommand").expect("ProxyCommand");

        assert!(proxy_command.contains("'ControlPath=/tmp/remux-%%r@%%h:%%p'"));
    }

    #[test]
    fn proxyjump_helper_escapes_destination_percent_tokens() {
        let mut options = BTreeMap::new();
        options.insert("ProxyJump".to_string(), "[fe80::1%en0]".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);
        let proxy_command = ssh_option(&args, "ProxyCommand").expect("ProxyCommand");

        assert!(proxy_command.contains("'fe80::1%%en0'"));
    }

    #[test]
    fn parses_proxyjump_hops_with_inline_ports_and_ipv6() {
        assert_eq!(
            super::parse_proxyjump_hop("user@bastion:2222"),
            Some(super::ProxyJumpHop {
                destination: "user@bastion".to_string(),
                port: Some("2222".to_string()),
            })
        );
        assert_eq!(
            super::parse_proxyjump_hop("user@[2001:db8::1]:2222"),
            Some(super::ProxyJumpHop {
                destination: "user@2001:db8::1".to_string(),
                port: Some("2222".to_string()),
            })
        );
        assert_eq!(
            super::parse_proxyjump_hop("bastion:ssh"),
            Some(super::ProxyJumpHop {
                destination: "bastion".to_string(),
                port: Some("ssh".to_string()),
            })
        );
        assert_eq!(
            super::parse_proxyjump_hop("ssh://jump"),
            Some(super::ProxyJumpHop {
                destination: "jump".to_string(),
                port: None,
            })
        );
        assert_eq!(
            super::parse_proxyjump_hop("ssh://jump/"),
            Some(super::ProxyJumpHop {
                destination: "jump".to_string(),
                port: None,
            })
        );
        assert_eq!(
            super::parse_proxyjump_hop("jump:"),
            Some(super::ProxyJumpHop {
                destination: "jump".to_string(),
                port: None,
            })
        );
    }

    #[test]
    fn explicit_proxycommand_overrides_interactive_proxyjump_conversion() {
        let mut options = BTreeMap::new();
        options.insert("ProxyCommand".to_string(), "custom proxy".to_string());
        options.insert("ProxyJump".to_string(), "jump".to_string());
        let host = ssh_host(options);

        let command = super::base_command(&host, Duration::from_secs(5), true).unwrap();
        let args = command_args(&command);

        assert!(has_ssh_option(&args, "ProxyCommand=custom proxy"));
        assert!(has_ssh_option(&args, "ProxyJump=jump"));
    }

    #[test]
    fn parses_proxyjump_from_ssh_g_output() {
        let proxy =
            super::parse_ssh_g_proxy("hostname final.example\nproxyjump jump\nproxycommand none\n");

        assert_eq!(proxy.as_deref(), Some("jump"));
    }

    #[test]
    fn ssh_g_proxycommand_takes_precedence_over_proxyjump() {
        let proxy =
            super::parse_ssh_g_proxy("proxyjump jump\nproxycommand ssh -W final.example:22 jump\n");

        assert_eq!(proxy, None);
    }

    fn ssh_host(options: BTreeMap<String, String>) -> HostConfig {
        ssh_host_with_config(options, PathBuf::from("/dev/null"))
    }

    fn ssh_host_with_config(options: BTreeMap<String, String>, config_file: PathBuf) -> HostConfig {
        HostConfig {
            id: "pi".to_string(),
            kind: HostKind::Ssh,
            tmux_socket: None,
            session_roots: Vec::new(),
            ssh: Some(SshConfig {
                target: Some("cam@192.168.0.197".to_string()),
                host: None,
                user: None,
                port: None,
                config_file: Some(config_file),
                options,
                remote_shell: None,
            }),
            container: None,
        }
    }

    fn command_args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn has_ssh_option(args: &[String], expected: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == "-o" && window[1] == expected)
    }

    fn ssh_option<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
        args.windows(2)
            .filter(|window| window[0] == "-o")
            .filter_map(|window| window[1].split_once('='))
            .find(|(option_key, _)| option_key.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }

    fn temp_ssh_config(contents: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "remux-ssh-test-{}-{nanos}.conf",
            std::process::id()
        ));
        fs::write(&path, contents).unwrap();
        path
    }
}
