use crate::config::{Config, HostKind};
use crate::host;
use crate::fzf;
use crate::{local, ssh};
use anyhow::Result;

const LOCAL_TMUX_CHECK: &str = "command -v tmux >/dev/null 2>&1";
const LOCAL_GIT_CHECK: &str = "command -v git >/dev/null 2>&1";
const SSH_TMUX_CHECK: &str = "command -v tmux >/dev/null 2>&1";
const SSH_GIT_CHECK: &str = "command -v git >/dev/null 2>&1";

pub fn run(config: &Config, json: bool) -> Result<()> {
    let report = DoctorReport::collect(config);
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_text(&report);
    }

    if report.ok {
        Ok(())
    } else {
        anyhow::bail!("doctor found issues")
    }
}

#[derive(Debug, serde::Serialize)]
struct DoctorReport {
    ok: bool,
    config_path_hint: String,
    local_tools: Vec<CheckResult>,
    hosts: Vec<HostDoctorReport>,
}

#[derive(Debug, serde::Serialize)]
struct HostDoctorReport {
    host: String,
    kind: String,
    target: String,
    ok: bool,
    checks: Vec<CheckResult>,
}

#[derive(Debug, serde::Serialize)]
struct CheckResult {
    name: String,
    ok: bool,
    detail: String,
}

impl DoctorReport {
    fn collect(config: &Config) -> Self {
        let local_tools = vec![
            check_local_binary(
                "local tmux",
                LOCAL_TMUX_CHECK,
                "tmux found",
                "tmux not found in PATH",
            ),
            check_local_binary(
                "local git",
                LOCAL_GIT_CHECK,
                "git found",
                "git not found in PATH",
            ),
            check_fzf(),
        ];

        let hosts: Vec<HostDoctorReport> = host::expand_docker_hosts(config)
            .iter()
            .map(|host| match host.kind {
                HostKind::Local => HostDoctorReport {
                    host: host.id.clone(),
                    kind: "local".to_string(),
                    target: "-".to_string(),
                    ok: true,
                    checks: vec![CheckResult {
                        name: "host access".to_string(),
                        ok: true,
                        detail: "local host".to_string(),
                    }],
                },
                HostKind::Ssh | HostKind::Docker => {
                    let kind = if host.kind == HostKind::Docker {
                        "docker"
                    } else {
                        "ssh"
                    };
                    let target = host
                        .ssh()
                        .ok()
                        .and_then(|ssh| ssh.target())
                        .unwrap_or_else(|| "-".to_string());
                    let access = check_ssh_command(
                        config,
                        host,
                        "ssh access",
                        "printf ok",
                        "ssh reachable",
                        "ssh failed",
                    );
                    let tmux = check_ssh_command(
                        config,
                        host,
                        "remote tmux",
                        SSH_TMUX_CHECK,
                        "tmux found",
                        "tmux not found on remote PATH",
                    );
                    let git = check_ssh_command(
                        config,
                        host,
                        "remote git",
                        SSH_GIT_CHECK,
                        "git found",
                        "git not found on remote PATH",
                    );
                    let checks = vec![access, tmux, git];
                    let ok = checks.iter().all(|check| check.ok);
                    HostDoctorReport {
                        host: host.id.clone(),
                        kind: kind.to_string(),
                        target,
                        ok,
                        checks,
                    }
                }
            })
            .collect();

        let ok = local_tools.iter().all(|check| check.ok) && hosts.iter().all(|host| host.ok);
        Self {
            ok,
            config_path_hint: "~/.config/remux/config.yaml".to_string(),
            local_tools,
            hosts,
        }
    }
}

fn check_local_binary(name: &str, command: &str, success: &str, failure: &str) -> CheckResult {
    match local::run(command, std::time::Duration::from_secs(2)) {
        Ok(_) => CheckResult {
            name: name.to_string(),
            ok: true,
            detail: success.to_string(),
        },
        Err(err) => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: format!("{failure}: {err:#}"),
        },
    }
}

fn check_fzf() -> CheckResult {
    match fzf::is_missing() {
        Ok(false) => CheckResult {
            name: "local fzf".to_string(),
            ok: true,
            detail: "fzf found".to_string(),
        },
        Ok(true) => CheckResult {
            name: "local fzf".to_string(),
            ok: false,
            detail: "fzf not found in PATH (only required for remux pick)".to_string(),
        },
        Err(err) => CheckResult {
            name: "local fzf".to_string(),
            ok: false,
            detail: format!("failed to check fzf: {err:#}"),
        },
    }
}

fn check_ssh_command(
    config: &Config,
    host: &crate::config::HostConfig,
    name: &str,
    command: &str,
    success: &str,
    failure: &str,
) -> CheckResult {
    match ssh::run(
        host,
        command,
        config.poll.ssh_timeout,
        config.poll.command_timeout,
    ) {
        Ok(output) => {
            let trimmed = output.trim();
            let detail = if trimmed.is_empty() {
                success.to_string()
            } else {
                format!("{success}: {trimmed}")
            };
            CheckResult {
                name: name.to_string(),
                ok: true,
                detail,
            }
        }
        Err(err) => CheckResult {
            name: name.to_string(),
            ok: false,
            detail: format!("{failure}: {err:#}"),
        },
    }
}

fn render_text(report: &DoctorReport) {
    println!("remux doctor");
    println!("config: {}", report.config_path_hint);
    println!();
    println!("LOCAL");
    for check in &report.local_tools {
        print_check(check);
    }
    println!();
    println!("HOSTS");
    for host in &report.hosts {
        if host.ok {
            println!("- {} [{}] {}", host.host, host.kind, host.target);
        } else {
            let issues = host.checks.iter().filter(|c| !c.ok).count();
            println!(
                "- {} [{}] {} ({} issue{})",
                host.host,
                host.kind,
                host.target,
                issues,
                if issues == 1 { "" } else { "s" }
            );
        }
        for check in &host.checks {
            print_check(check);
        }
    }
    println!();
    println!("overall: {}", if report.ok { "ok" } else { "fail" });
}

fn print_check(check: &CheckResult) {
    let mark = if check.ok { "OK" } else { "FAIL" };
    println!("  {mark:<4} {:<16} {}", check.name, check.detail);
}
