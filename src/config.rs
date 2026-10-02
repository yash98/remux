use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub poll: PollConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub launch_templates: LaunchTemplatesConfig,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
    #[serde(default)]
    pub watches: Vec<WatchConfig>,
    #[serde(default)]
    pub sessions: Vec<SessionConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TuiConfig {
    #[serde(default)]
    pub sort: TuiSortConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TuiSortConfig {
    #[serde(default)]
    pub field: TuiSortField,
    #[serde(default)]
    pub direction: TuiSortDirection,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TuiSortField {
    #[default]
    Attention,
    LastOutput,
    State,
    Id,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TuiSortDirection {
    Asc,
    #[default]
    Desc,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PollConfig {
    #[serde(
        default = "default_active_after",
        deserialize_with = "deserialize_duration"
    )]
    pub active_after: Duration,
    #[serde(
        default = "default_idle_after",
        deserialize_with = "deserialize_duration"
    )]
    pub idle_after: Duration,
    #[serde(default = "default_capture_lines")]
    pub capture_lines: usize,
    #[serde(
        default = "default_ssh_timeout",
        deserialize_with = "deserialize_duration"
    )]
    pub ssh_timeout: Duration,
    #[serde(
        default = "default_command_timeout",
        deserialize_with = "deserialize_duration"
    )]
    pub command_timeout: Duration,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    #[serde(default = "default_collect_git")]
    pub collect_git: bool,
    #[serde(
        default = "default_git_cache_ttl",
        deserialize_with = "deserialize_duration"
    )]
    pub git_cache_ttl: Duration,
    /// Interval between automatic background refreshes in the TUI. Set to 0s to disable and rely on manual [r] refreshes.
    #[serde(
        default = "default_auto_refresh_interval",
        deserialize_with = "deserialize_duration"
    )]
    pub auto_refresh_interval: Duration,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LaunchTemplatesConfig {
    #[serde(default)]
    pub presets: Vec<LaunchTemplatePresetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LaunchTemplatePresetConfig {
    pub id: String,
    pub label: String,
    pub session_prefix: String,
    pub command: String,
    #[serde(default)]
    pub window_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: HostKind,
    #[serde(default)]
    pub tmux_socket: Option<String>,
    #[serde(default)]
    pub session_roots: Vec<String>,
    pub ssh: Option<SshConfig>,
    /// Container name for `type: docker` hosts. Commands are run as
    /// `ssh <target> docker exec -i <container> <command>`, so tmux lives
    /// inside the container.
    #[serde(default)]
    pub container: Option<String>,
    /// Substrings to match against `docker ps --format {{.Names}}` run on the
    /// ssh target at config load; the entry expands into one docker host per
    /// matching container (ids `<id>-<container>`).
    #[serde(default)]
    pub container_filter: Vec<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HostKind {
    Local,
    Ssh,
    Docker,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SshConfig {
    pub target: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub config_file: Option<PathBuf>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Wrap the remote command in this shell invocation, e.g.
    /// `["bash", "-lc"]` to run via a login shell so `~/.bashrc`/`~/.zprofile`
    /// PATH tweaks are picked up. The remote command is appended as the final
    /// argument.
    #[serde(default)]
    pub remote_shell: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionConfig {
    pub id: String,
    pub host: String,
    pub tmux: TmuxTarget,
    pub repo: Option<String>,
    pub agent_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmuxTarget {
    pub session: String,
    pub window: Option<u32>,
    pub pane: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchConfig {
    pub id: String,
    pub host: String,
    #[serde(rename = "match")]
    pub matcher: WatchMatchConfig,
    pub repo: Option<String>,
    pub agent_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WatchMatchConfig {
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub cwd_prefix: Option<String>,
    pub tmux: Option<TmuxTarget>,
}

#[derive(Debug, Clone)]
pub struct Watch {
    pub id: String,
    pub host: String,
    pub matcher: WatchMatchConfig,
    pub repo: Option<String>,
    pub agent_hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct IndexedWatch {
    pub index: usize,
    pub watch: Watch,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            active_after: default_active_after(),
            idle_after: default_idle_after(),
            capture_lines: default_capture_lines(),
            ssh_timeout: default_ssh_timeout(),
            command_timeout: default_command_timeout(),
            max_concurrency: default_max_concurrency(),
            collect_git: default_collect_git(),
            git_cache_ttl: default_git_cache_ttl(),
            auto_refresh_interval: default_auto_refresh_interval(),
        }
    }
}

impl Default for TuiSortConfig {
    fn default() -> Self {
        Self {
            field: TuiSortField::Attention,
            direction: TuiSortDirection::Desc,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(path) => expand_home_path(path),
            None => default_config_path()?,
        };
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        let config: Config = yaml_serde::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn host(&self, id: &str) -> Result<&HostConfig> {
        self.hosts
            .iter()
            .find(|host| host.id == id)
            .ok_or_else(|| anyhow!("unknown host `{id}`"))
    }

    /// Like `host`, but also resolves virtual docker hosts created at poll
    /// time by `container_filter` (ids of the form `<id>-<container>`).
    pub fn resolve_host(&self, id: &str) -> Result<HostConfig> {
        if let Ok(host) = self.host(id) {
            return Ok(host.clone());
        }
        for host in &self.hosts {
            if host.kind != HostKind::Docker || host.container_filter.is_empty() {
                continue;
            }
            if let Some(container) = id.strip_prefix(&format!("{}-", host.id)) {
                let mut resolved = host.clone();
                resolved.id = id.to_string();
                resolved.container = Some(container.to_string());
                return Ok(resolved);
            }
        }
        bail!("unknown host `{id}`")
    }

    pub fn watch(&self, id: &str) -> Result<IndexedWatch> {
        self.indexed_watches()
            .into_iter()
            .find(|watch| watch.watch.id == id)
            .ok_or_else(|| anyhow!("unknown watch `{id}`"))
    }

    pub fn find_watch(&self, id: &str) -> Option<IndexedWatch> {
        self.indexed_watches()
            .into_iter()
            .find(|watch| watch.watch.id == id)
    }

    pub fn watches_for_host(&self, host_id: &str) -> Vec<IndexedWatch> {
        self.indexed_watches()
            .into_iter()
            .filter(|watch| watch.watch.host == host_id)
            .collect()
    }

    pub fn indexed_watches(&self) -> Vec<IndexedWatch> {
        self.watches
            .iter()
            .map(Watch::from_watch_config)
            .chain(self.sessions.iter().map(Watch::from_session_config))
            .enumerate()
            .map(|(index, watch)| IndexedWatch { index, watch })
            .collect()
    }

    fn validate(&self) -> Result<()> {
        if self.poll.active_after.is_zero() {
            bail!("poll.active_after must be greater than zero");
        }
        if self.poll.idle_after < self.poll.active_after {
            bail!("poll.idle_after must be greater than or equal to poll.active_after");
        }
        if self.poll.capture_lines == 0 {
            bail!("poll.capture_lines must be greater than zero");
        }
        if self.poll.ssh_timeout.is_zero() {
            bail!("poll.ssh_timeout must be greater than zero");
        }
        if self.poll.command_timeout.is_zero() {
            bail!("poll.command_timeout must be greater than zero");
        }
        if self.poll.max_concurrency == 0 {
            bail!("poll.max_concurrency must be greater than zero");
        }
        if self.poll.git_cache_ttl.is_zero() {
            bail!("poll.git_cache_ttl must be greater than zero");
        }
        if !self.poll.auto_refresh_interval.is_zero()
            && self.poll.auto_refresh_interval < Duration::from_secs(1)
        {
            bail!("poll.auto_refresh_interval must be 0s (disabled) or at least 1s");
        }
        validate_launch_templates(&self.launch_templates)?;

        let mut host_ids = HashSet::new();
        for host in &self.hosts {
            if host.id.trim().is_empty() {
                bail!("host id must not be empty");
            }
            if !host_ids.insert(host.id.as_str()) {
                bail!("duplicate host id `{}`", host.id);
            }
            if host
                .tmux_socket
                .as_ref()
                .is_some_and(|socket| socket.trim().is_empty())
            {
                bail!("host `{}` tmux_socket must not be empty", host.id);
            }
            if host.session_roots.iter().any(|root| root.trim().is_empty()) {
                bail!("host `{}` session_roots entries must not be empty", host.id);
            }

            match host.kind {
                HostKind::Local => {
                    if host.ssh.is_some() {
                        bail!("local host `{}` must not include ssh config", host.id);
                    }
                }
                HostKind::Ssh | HostKind::Docker => {
                    let ssh = host.ssh.as_ref().ok_or_else(|| {
                        anyhow!(
                            "host `{}` is type {} but is missing ssh config",
                            host.id,
                            if host.kind == HostKind::Docker {
                                "docker"
                            } else {
                                "ssh"
                            }
                        )
                    })?;
                    if ssh.target().is_none() {
                        bail!(
                            "host `{}` is type ssh but is missing ssh.target or ssh.host",
                            host.id
                        );
                    }
                    if host.kind == HostKind::Docker
                        && host
                            .container
                            .as_deref()
                            .map(str::trim)
                            .is_none_or(str::is_empty)
                        && host.container_filter.iter().all(|f| f.trim().is_empty())
                    {
                        bail!("docker host `{}` is missing container", host.id);
                    }
                }
            }
        }

        let mut watch_ids = HashSet::new();
        for watch in &self.watches {
            validate_watch(
                &mut watch_ids,
                &host_ids,
                &watch.id,
                &watch.host,
                &watch.matcher,
            )?;
        }

        let mut session_ids = HashSet::new();
        for session in &self.sessions {
            if session.id.trim().is_empty() {
                bail!("session id must not be empty");
            }
            if !session_ids.insert(session.id.as_str()) {
                bail!("duplicate session id `{}`", session.id);
            }
            if !host_ids.contains(session.host.as_str()) {
                bail!(
                    "session `{}` references missing host `{}`",
                    session.id,
                    session.host
                );
            }
            if session.tmux.session.trim().is_empty() {
                bail!("session `{}` tmux.session must not be empty", session.id);
            }
            if !watch_ids.insert(session.id.as_str()) {
                bail!(
                    "duplicate watch/session id `{}`; session ids and watch ids share one namespace",
                    session.id
                );
            }
        }

        Ok(())
    }
}

impl Watch {
    fn from_watch_config(config: &WatchConfig) -> Self {
        Self {
            id: config.id.clone(),
            host: config.host.clone(),
            matcher: config.matcher.clone(),
            repo: config.repo.clone(),
            agent_hint: config.agent_hint.clone(),
        }
    }

    fn from_session_config(config: &SessionConfig) -> Self {
        Self {
            id: config.id.clone(),
            host: config.host.clone(),
            matcher: WatchMatchConfig {
                command: None,
                cwd: None,
                cwd_prefix: None,
                tmux: Some(config.tmux.clone()),
            },
            repo: config.repo.clone(),
            agent_hint: config.agent_hint.clone(),
        }
    }
}

impl HostConfig {
    pub fn is_local(&self) -> bool {
        self.kind == HostKind::Local
    }

    pub fn tmux_socket(&self) -> Option<&str> {
        self.tmux_socket
            .as_deref()
            .map(str::trim)
            .filter(|socket| !socket.is_empty())
    }

    pub fn ssh(&self) -> Result<&SshConfig> {
        self.ssh
            .as_ref()
            .ok_or_else(|| anyhow!("host `{}` is missing ssh config", self.id))
    }

    /// Container name for `type: docker` hosts; commands are wrapped in
    /// `docker exec -i <container>` over ssh.
    pub fn container(&self) -> Option<&str> {
        self.container
            .as_deref()
            .map(str::trim)
            .filter(|container| !container.is_empty())
    }
}

impl SshConfig {
    pub fn target(&self) -> Option<String> {
        if let Some(target) = &self.target {
            return Some(target.clone());
        }

        let host = self.host.as_ref()?;
        Some(match &self.user {
            Some(user) => format!("{user}@{host}"),
            None => host.clone(),
        })
    }

    pub fn ssh_options(&self, default_timeout: Duration) -> BTreeMap<String, String> {
        let timeout = default_timeout.as_secs().max(1).to_string();
        let mut options = BTreeMap::from([
            ("BatchMode".to_string(), "yes".to_string()),
            ("ConnectTimeout".to_string(), timeout),
            ("ServerAliveInterval".to_string(), "3".to_string()),
            ("ServerAliveCountMax".to_string(), "2".to_string()),
        ]);
        options.extend(self.options.clone());
        options
    }
}

pub fn expand_home_path(path: &Path) -> PathBuf {
    let display = path.to_string_lossy();
    if display == "~" {
        return std::env::var_os("HOME").map_or_else(|| path.to_path_buf(), PathBuf::from);
    }
    if let Some(rest) = display.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".config/remux/config.yaml"))
}

pub fn resolve_config_path(path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(expand_home_path(path)),
        None => default_config_path(),
    }
}

fn default_active_after() -> Duration {
    Duration::from_secs(5 * 60)
}

fn default_idle_after() -> Duration {
    Duration::from_secs(60 * 60)
}

fn default_capture_lines() -> usize {
    120
}

fn default_ssh_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_command_timeout() -> Duration {
    Duration::from_secs(15)
}

fn default_max_concurrency() -> usize {
    4
}

fn default_collect_git() -> bool {
    true
}

fn default_git_cache_ttl() -> Duration {
    Duration::from_secs(30)
}

fn default_auto_refresh_interval() -> Duration {
    Duration::from_secs(15)
}

pub const BUILTIN_LAUNCH_TEMPLATE_IDS: &[&str] = &["pi"];

fn validate_launch_templates(config: &LaunchTemplatesConfig) -> Result<()> {
    let mut ids = HashSet::new();
    for preset in &config.presets {
        let id = preset.id.trim();
        if id.is_empty() {
            bail!("launch template preset id must not be empty");
        }
        if BUILTIN_LAUNCH_TEMPLATE_IDS.contains(&id) {
            bail!("launch template preset `{id}` conflicts with a built-in preset");
        }
        if !ids.insert(id) {
            bail!("duplicate launch template preset id `{id}`");
        }
        if preset.label.trim().is_empty() {
            bail!("launch template preset `{id}` label must not be empty");
        }
        let prefix = preset.session_prefix.trim();
        if prefix.is_empty() {
            bail!("launch template preset `{id}` session_prefix must not be empty");
        }
        if prefix.contains('/') || prefix.contains(':') {
            bail!("launch template preset `{id}` session_prefix must not contain `/` or `:`");
        }
        if preset.command.trim().is_empty() {
            bail!("launch template preset `{id}` command must not be empty");
        }
        if preset
            .window_name
            .as_ref()
            .is_some_and(|window_name| window_name.trim().is_empty())
        {
            bail!("launch template preset `{id}` window_name must not be empty");
        }
    }
    Ok(())
}

fn validate_watch<'a>(
    seen_ids: &mut HashSet<&'a str>,
    host_ids: &HashSet<&str>,
    id: &'a str,
    host: &str,
    matcher: &WatchMatchConfig,
) -> Result<()> {
    if id.trim().is_empty() {
        bail!("watch id must not be empty");
    }
    if !seen_ids.insert(id) {
        bail!("duplicate watch id `{id}`");
    }
    if !host_ids.contains(host) {
        bail!("watch `{id}` references missing host `{host}`");
    }
    if matcher.command.is_none()
        && matcher.cwd.is_none()
        && matcher.cwd_prefix.is_none()
        && matcher.tmux.is_none()
    {
        bail!("watch `{id}` match must not be empty");
    }
    if matcher.cwd.is_some() && matcher.cwd_prefix.is_some() {
        bail!("watch `{id}` must not set both match.cwd and match.cwd_prefix");
    }
    if matcher
        .command
        .as_ref()
        .is_some_and(|command| command.trim().is_empty())
    {
        bail!("watch `{id}` match.command must not be empty");
    }
    if matcher
        .cwd
        .as_ref()
        .is_some_and(|cwd| cwd.trim().is_empty())
    {
        bail!("watch `{id}` match.cwd must not be empty");
    }
    if matcher
        .cwd_prefix
        .as_ref()
        .is_some_and(|cwd| cwd.trim().is_empty())
    {
        bail!("watch `{id}` match.cwd_prefix must not be empty");
    }
    if let Some(tmux) = &matcher.tmux
        && tmux.session.trim().is_empty()
    {
        bail!("watch `{id}` match.tmux.session must not be empty");
    }
    Ok(())
}

fn deserialize_duration<'de, D>(deserializer: D) -> std::result::Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_duration(&value).map_err(serde::de::Error::custom)
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("duration must not be empty".to_string());
    }

    let split_at = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split_at);
    let amount: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration amount `{value}`"))?;
    let seconds = match unit {
        "" | "s" => amount,
        "m" => amount * 60,
        "h" => amount * 60 * 60,
        _ => return Err(format!("invalid duration unit `{unit}` in `{value}`")),
    };
    Ok(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_with_hosts_sessions_and_poll() {
        let config: Config = yaml_serde::from_str(
            r#"
poll:
  active_after: 5m
  idle_after: 60m
  capture_lines: 80
  ssh_timeout: 7s
  command_timeout: 11s
  max_concurrency: 3
tui:
  sort:
    field: last-output
    direction: asc
hosts:
  - id: local
    type: local
  - id: pi
    type: ssh
    tmux_socket: ~/.work-os/tmux.sock
    session_roots:
      - /home/cam/work
      - ~/code
    ssh:
      target: cam@192.168.0.197
launch_templates:
  presets:
    - id: agent
      label: Coding Agent
      session_prefix: agent
      command: pi
      window_name: agent
sessions:
  - id: agent
    host: pi
    tmux:
      session: work
      window: 0
      pane: 1
    repo: ~/work
    agent_hint: codex
watches:
  - id: pi-cwd-agent
    host: pi
    match:
      command: node
      cwd_prefix: /home/cam/work
    agent_hint: codex
"#,
        )
        .unwrap();

        config.validate().unwrap();
        assert_eq!(config.poll.capture_lines, 80);
        assert_eq!(config.poll.command_timeout, Duration::from_secs(11));
        assert_eq!(config.poll.max_concurrency, 3);
        assert_eq!(config.tui.sort.field, TuiSortField::LastOutput);
        assert_eq!(config.tui.sort.direction, TuiSortDirection::Asc);
        assert_eq!(
            config.host("pi").unwrap().tmux_socket(),
            Some("~/.work-os/tmux.sock")
        );
        assert_eq!(
            config.host("pi").unwrap().session_roots,
            vec!["/home/cam/work", "~/code"]
        );
        assert_eq!(
            config.host("pi").unwrap().ssh().unwrap().target().unwrap(),
            "cam@192.168.0.197"
        );
        assert_eq!(config.launch_templates.presets.len(), 1);
        assert_eq!(config.launch_templates.presets[0].session_prefix, "agent");
        assert_eq!(config.launch_templates.presets[0].command, "pi");
        assert_eq!(
            config.launch_templates.presets[0].window_name.as_deref(),
            Some("agent")
        );
        assert_eq!(
            config.watch("agent").unwrap().watch.agent_hint.as_deref(),
            Some("codex")
        );
        assert_eq!(config.watches_for_host("pi").len(), 2);
    }

    #[test]
    fn tui_sort_defaults_to_attention_desc() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: local
    type: local
"#,
        )
        .unwrap();

        assert_eq!(config.tui.sort.field, TuiSortField::Attention);
        assert_eq!(config.tui.sort.direction, TuiSortDirection::Desc);
    }

    #[test]
    fn rejects_invalid_tui_sort_values() {
        let err = yaml_serde::from_str::<Config>(
            r#"
tui:
  sort:
    field: newest
    direction: sideways
"#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("newest"));
    }

    #[test]
    fn rejects_empty_session_roots() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: local
    type: local
    session_roots:
      - " "
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("session_roots entries must not be empty")
        );
    }

    #[test]
    fn rejects_duplicate_launch_template_presets() {
        let config: Config = yaml_serde::from_str(
            r#"
launch_templates:
  presets:
    - id: agent
      label: Agent
      session_prefix: agent
      command: pi
    - id: agent
      label: Other Agent
      session_prefix: other
      command: codex
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate launch template preset id `agent`")
        );
    }

    #[test]
    fn rejects_launch_template_presets_conflicting_with_builtins() {
        let config: Config = yaml_serde::from_str(
            r#"
launch_templates:
  presets:
    - id: pi
      label: Custom Pi
      session_prefix: custom-pi
      command: custom-pi
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("conflicts with a built-in preset")
        );
    }

    #[test]
    fn rejects_empty_launch_template_preset_fields() {
        let config: Config = yaml_serde::from_str(
            r#"
launch_templates:
  presets:
    - id: agent
      label: Agent
      session_prefix: agent
      command: " "
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("command must not be empty")
        );
    }

    #[test]
    fn rejects_invalid_launch_template_prefix_chars() {
        let config: Config = yaml_serde::from_str(
            r#"
launch_templates:
  presets:
    - id: agent
      label: Agent
      session_prefix: agent/work
      command: pi
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("session_prefix must not contain `/` or `:`")
        );
    }

    #[test]
    fn rejects_empty_tmux_socket() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: local
    type: local
    tmux_socket: " "
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("tmux_socket must not be empty")
        );
    }

    #[test]
    fn rejects_duplicate_hosts() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: pi
    type: ssh
    ssh: { target: cam@192.168.0.197 }
  - id: pi
    type: ssh
    ssh: { target: cam@192.168.0.198 }
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate host id")
        );
    }

    #[test]
    fn rejects_duplicate_sessions() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: local
    type: local
sessions:
  - id: agent
    host: local
    tmux: { session: one }
  - id: agent
    host: local
    tmux: { session: two }
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate session id")
        );
    }

    #[test]
    fn rejects_missing_host_reference() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: local
    type: local
sessions:
  - id: agent
    host: missing
    tmux: { session: one }
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("missing host")
        );
    }

    #[test]
    fn rejects_invalid_watch_match() {
        let config: Config = yaml_serde::from_str(
            r#"
hosts:
  - id: local
    type: local
watches:
  - id: agent
    host: local
    match:
      cwd: /tmp
      cwd_prefix: /tmp/work
"#,
        )
        .unwrap();

        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("must not set both")
        );
    }

    #[test]
    fn parses_duration_units() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn auto_refresh_interval_defaults_to_15s() {
        let config: Config = yaml_serde::from_str("{}").unwrap();
        assert_eq!(config.poll.auto_refresh_interval, Duration::from_secs(15));
    }

    #[test]
    fn auto_refresh_interval_parses_15s() {
        let config: Config = yaml_serde::from_str("poll:\n  auto_refresh_interval: 15s").unwrap();
        assert_eq!(config.poll.auto_refresh_interval, Duration::from_secs(15));
        config.validate().unwrap();
    }

    #[test]
    fn auto_refresh_interval_parses_0s() {
        let config: Config = yaml_serde::from_str("poll:\n  auto_refresh_interval: 0s").unwrap();
        assert_eq!(config.poll.auto_refresh_interval, Duration::ZERO);
        config.validate().unwrap();
    }

    #[test]
    fn auto_refresh_interval_rejects_500ms() {
        // parse_duration only supports whole-second units, so "500ms" is an
        // invalid unit string and is rejected at parse time.
        let result: Result<Config, _> =
            yaml_serde::from_str("poll:\n  auto_refresh_interval: 500ms");
        assert!(result.is_err());
    }
}
