use crate::config::{Config, HostConfig, HostKind};
use crate::{local, ssh};
use anyhow::Result;

pub fn run(config: &Config, host: &HostConfig, command: &str) -> Result<String> {
    match host.kind {
        HostKind::Local => local::run(command, config.poll.command_timeout),
        HostKind::Ssh => ssh::run(
            host,
            command,
            config.poll.ssh_timeout,
            config.poll.command_timeout,
        ),
        HostKind::Docker => {
            if host.container().is_none() {
                // container_filter host that failed discovery, or matched nothing
                anyhow::bail!("host `{}`: no matching containers found", host.id);
            }
            ssh::run(
                host,
                command,
                config.poll.ssh_timeout,
                config.poll.command_timeout,
            )
        }
    }
}

/// Expand docker hosts with `container_filter` into one concrete docker host
/// per matching container (ids `<id>-<container>`), running `docker ps` on each
/// filtered host. Hosts that fail to poll are kept as-is and surface as
/// unreachable.
pub fn expand_docker_hosts(config: &Config) -> Vec<HostConfig> {
    let mut out = Vec::new();
    for host in &config.hosts {
        if host.kind != HostKind::Docker
            || host.container.is_some()
            || host.container_filter.is_empty()
        {
            out.push(host.clone());
            continue;
        }
        let output = match ssh::run(
            host,
            "docker ps --format \"{{.Names}}\"",
            config.poll.ssh_timeout,
            config.poll.command_timeout,
        ) {
            Ok(output) => output,
            Err(_) => {
                out.push(host.clone());
                continue;
            }
        };
        let names: Vec<String> = output
            .lines()
            .map(str::trim)
            .filter(|name| {
                !name.is_empty()
                    && host
                        .container_filter
                        .iter()
                        .any(|filter| name.contains(filter.as_str()))
            })
            .map(str::to_string)
            .collect();
        let mut expanded = names
            .into_iter()
            .map(|name| {
                let mut h = host.clone();
                h.id = format!("{}-{}", host.id, name);
                h.container = Some(name.to_string());
                h
            })
            .collect::<Vec<_>>();
        if expanded.is_empty() {
            out.push(host.clone());
        } else {
            out.append(&mut expanded);
        }
    }
    out
}

