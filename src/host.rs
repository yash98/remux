use crate::config::{Config, HostConfig, HostKind};
use crate::{local, ssh};
use anyhow::Result;

pub fn run(config: &Config, host: &HostConfig, command: &str) -> Result<String> {
    match host.kind {
        HostKind::Local => local::run(command, config.poll.command_timeout),
        HostKind::Ssh | HostKind::Docker => ssh::run(
            host,
            command,
            config.poll.ssh_timeout,
            config.poll.command_timeout,
        ),
    }
}
