use crate::cache::Cache;
use crate::config::{Config, HostConfig, IndexedWatch, Watch};
use crate::git::{self, RepoSnapshot};
use crate::host;
use crate::tmux::{self, Pane, PaneTarget};
use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

mod git_cache;

/// In-memory git cache shared across poll workers.
/// Key: (host_id, cwd) → (repo snapshot, time fetched).
pub type GitCache = Arc<Mutex<HashMap<(String, String), (Option<RepoSnapshot>, Instant)>>>;

static GLOBAL_GIT_CACHE: OnceLock<GitCache> = OnceLock::new();

fn global_git_cache() -> &'static GitCache {
    GLOBAL_GIT_CACHE.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

#[derive(Debug, Clone, Serialize)]
pub struct HostSnapshot {
    pub host: String,
    pub tmux_socket: Option<String>,
    pub status: SnapshotStatus,
    pub collected_at: DateTime<Utc>,
    pub sessions: Vec<SessionSnapshot>,
    pub errors: Vec<SnapshotError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotStatus {
    Ok,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchStatus {
    Matched,
    Orphan,
    Missing,
    Ambiguous,
    Shadowed,
    Unreachable,
    #[allow(dead_code)]
    Unknown,
}

impl MatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchStatus::Matched => "matched",
            MatchStatus::Orphan => "orphan",
            MatchStatus::Missing => "missing",
            MatchStatus::Ambiguous => "ambiguous",
            MatchStatus::Shadowed => "shadowed",
            MatchStatus::Unreachable => "unreachable",
            MatchStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Quiet,
    Idle,
    Missing,
    Unreachable,
    Unknown,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionState::Active => "active",
            SessionState::Quiet => "quiet",
            SessionState::Idle => "idle",
            SessionState::Missing => "missing",
            SessionState::Unreachable => "unreachable",
            SessionState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub target: String,
    pub display_id: String,
    pub raw_target: Option<String>,
    pub host: String,
    pub tmux_socket: Option<String>,
    pub match_status: MatchStatus,
    pub watch_id: Option<String>,
    pub watch_index: Option<usize>,
    pub candidate_targets: Vec<String>,
    pub shadowed_by: Option<String>,
    pub state: SessionState,
    pub agent_hint: Option<String>,
    pub tmux: TmuxSnapshot,
    pub process: Option<ProcessSnapshot>,
    pub repo: Option<RepoSnapshot>,
    pub output: Option<OutputSnapshot>,
    pub errors: Vec<SnapshotError>,
}

impl SessionSnapshot {
    pub fn display_command(&self) -> Option<String> {
        let process = self.process.as_ref()?;
        if agent_from_command(&process.command).is_some() {
            return Some(process.command.clone());
        }

        if let Some(agent) = agent_from_labels(
            self.tmux.window_name.as_deref(),
            self.tmux.pane_title.as_deref(),
        ) {
            return Some(agent.to_string());
        }
        Some(process.command.clone())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TmuxSnapshot {
    pub session: String,
    pub window: Option<String>,
    pub pane: Option<String>,
    pub pane_id: Option<String>,
    pub session_attached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_short: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessSnapshot {
    pub pid: Option<u32>,
    pub command: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputSnapshot {
    pub preview: String,
    pub recent: String,
    pub hash: String,
    pub last_output_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotError {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PaneDetail {
    #[serde(flatten)]
    pub session: SessionSnapshot,
    pub recent_output: String,
}

#[derive(Debug, Clone)]
struct WatchResolution {
    watch: IndexedWatch,
    status: MatchStatus,
    pane_index: Option<usize>,
    candidate_targets: Vec<String>,
    shadowed_by: Option<String>,
}

struct PaneRowContext<'a> {
    watch: Option<&'a IndexedWatch>,
    match_status: MatchStatus,
    candidate_targets: Vec<String>,
    shadowed_by: Option<String>,
}

struct SnapshotBuildContext<'a> {
    config: &'a Config,
    host_config: &'a HostConfig,
    captures: &'a HashMap<String, Option<String>>,
    git_map: &'a HashMap<String, Option<RepoSnapshot>>,
    git_cache: &'a GitCache,
    host_id: &'a str,
    cache: &'a mut Cache,
    now: DateTime<Utc>,
}

pub fn snapshot_host(config: &Config, host_id: &str) -> Result<HostSnapshot> {
    snapshot_host_with_persistent_cache(config, host_id)
}

fn snapshot_host_with_persistent_cache(config: &Config, host_id: &str) -> Result<HostSnapshot> {
    let cache_load = Cache::load_with_warning();
    let mut cache = cache_load.cache;
    let mut snapshot = snapshot_host_with_cache(config, host_id, &mut cache, global_git_cache())?;
    append_cache_warning(&mut snapshot, cache_load.warning);
    append_cache_save_error(&mut snapshot, cache.save().err());
    Ok(snapshot)
}

pub fn snapshot_all(config: &Config) -> Result<Vec<HostSnapshot>> {
    let cache_load = Cache::load_with_warning();
    let mut cache = cache_load.cache;
    let git_cache = global_git_cache();
    let mut snapshots = Vec::new();
    for host in host::expand_docker_hosts(config) {
        snapshots.push(snapshot_host_with_cache(
            config, &host.id, &mut cache, git_cache,
        )?);
    }
    if let Some(first) = snapshots.first_mut() {
        append_cache_warning(first, cache_load.warning);
        append_cache_save_error(first, cache.save().err());
    }
    Ok(snapshots)
}

pub fn snapshot_selected(config: &Config, host: Option<&str>) -> Result<Vec<HostSnapshot>> {
    match host {
        Some(host) => Ok(vec![snapshot_host(config, host)?]),
        None => snapshot_all(config),
    }
}

#[allow(dead_code)]
pub fn inspect(config: &Config, id_or_target: &str) -> Result<PaneDetail> {
    inspect_with_color(config, id_or_target, false)
}

pub fn inspect_with_color(config: &Config, id_or_target: &str, color: bool) -> Result<PaneDetail> {
    if config.find_watch(id_or_target).is_some() {
        return inspect_watch(config, id_or_target, color);
    }
    if let Some(target) = parse_pane_target_or_explain(id_or_target)? {
        return inspect_target(config, &target, color);
    }

    bail!("{}", unknown_watch_or_target_message(id_or_target))
}

/// Refresh the capture for an already-known pane without re-running host inventory.
/// Used by the TUI background inspect task.
pub fn refresh_capture_for(config: &Config, snapshot: &SessionSnapshot) -> Result<PaneDetail> {
    let target = target_for_snapshot(snapshot)?;
    let recent_output =
        capture_pane(config, &target, config.poll.capture_lines, false).unwrap_or_default();
    Ok(PaneDetail {
        session: snapshot.clone(),
        recent_output,
    })
}

pub fn capture(config: &Config, id_or_target: &str, lines: usize, color: bool) -> Result<String> {
    if lines == 0 {
        bail!("capture lines must be greater than zero");
    }
    let target = target_for_action(config, id_or_target, "capture")?;
    capture_pane(config, &target, lines, color)
}

pub fn capture_pane(
    config: &Config,
    target: &PaneTarget,
    lines: usize,
    color: bool,
) -> Result<String> {
    if lines == 0 {
        bail!("capture lines must be greater than zero");
    }
    let host_config = config.resolve_host(&target.host)?;
    let command = tmux::capture_command(target, lines, color, host_config.tmux_socket());
    host::run(config, &host_config, &command)
}

pub fn target_for_action(config: &Config, id_or_target: &str, action: &str) -> Result<PaneTarget> {
    if config.find_watch(id_or_target).is_some() {
        return target_for_watch(config, id_or_target, action);
    }
    if let Some(target) = parse_pane_target_or_explain(id_or_target)? {
        return Ok(target);
    }

    bail!("{}", unknown_watch_or_target_message(id_or_target))
}

fn parse_pane_target_or_explain(input: &str) -> Result<Option<PaneTarget>> {
    match PaneTarget::parse(input) {
        Ok(target) => Ok(Some(target)),
        Err(err) if input.contains('/') || input.contains(':') => {
            bail!(
                "invalid pane target `{input}`: {err:#}; expected <host>/<session>:<window>.<pane>, for example pi/work:0.1"
            )
        }
        Err(_) => Ok(None),
    }
}

fn unknown_watch_or_target_message(input: &str) -> String {
    format!(
        "unknown watch or pane target `{input}`; run `remux list` to see watch ids and pane targets"
    )
}

fn inspect_watch(config: &Config, watch_id: &str, color: bool) -> Result<PaneDetail> {
    let watch = config.watch(watch_id)?;
    let host_snapshot = snapshot_host_with_persistent_cache(config, &watch.watch.host)?;
    let mut snapshot = host_snapshot
        .sessions
        .into_iter()
        .find(|snapshot| snapshot.watch_id.as_deref() == Some(watch_id))
        .ok_or_else(|| anyhow!("watch `{watch_id}` was not found in snapshot"))?;

    if snapshot.match_status != MatchStatus::Matched {
        return Ok(PaneDetail {
            session: snapshot,
            recent_output: String::new(),
        });
    }

    let target = target_for_snapshot(&snapshot)?;
    let recent_output = capture_for_inspect(config, &target, &mut snapshot, color)?;
    Ok(PaneDetail {
        session: snapshot,
        recent_output,
    })
}

fn inspect_target(config: &Config, target: &PaneTarget, color: bool) -> Result<PaneDetail> {
    let host_snapshot = snapshot_host_with_persistent_cache(config, &target.host)?;
    let target_string = target.to_string();
    let mut snapshot = host_snapshot
        .sessions
        .into_iter()
        .find(|snapshot| snapshot.raw_target.as_deref() == Some(target_string.as_str()))
        .ok_or_else(|| anyhow!("pane `{target}` was not found on host `{}`", target.host))?;
    let recent_output = capture_for_inspect(config, target, &mut snapshot, color)?;
    Ok(PaneDetail {
        session: snapshot,
        recent_output,
    })
}

fn snapshot_host_with_cache(
    config: &Config,
    host_id: &str,
    cache: &mut Cache,
    git_cache: &GitCache,
) -> Result<HostSnapshot> {
    let host_config = config.resolve_host(host_id)?;
    let now = Utc::now();

    // Determine which cwds are cache-fresh so we can skip them in the git section.
    let skip_git_cwds = git_cache::fresh_cwds_for_host(
        git_cache,
        host_id,
        config.poll.git_cache_ttl,
        config.poll.collect_git,
    );

    let command = tmux::inventory_with_captures_command(
        config.poll.capture_lines,
        config.poll.collect_git,
        &skip_git_cwds,
        host_config.tmux_socket(),
    );
    match host::run(config, &host_config, &command) {
        Ok(output) => {
            let (panes, captures, git_map) = tmux::parse_inventory_with_captures(host_id, &output)?;

            // Update git cache with fresh results; use cached value for skipped cwds.
            for (pane_id, repo) in &git_map {
                if let Some(pane) = panes.iter().find(|p| &p.pane_id == pane_id) {
                    git_cache::insert(
                        git_cache,
                        (host_id.to_string(), pane.cwd.clone()),
                        repo.clone(),
                    );
                }
            }

            let mut build = SnapshotBuildContext {
                config,
                host_config: &host_config,
                captures: &captures,
                git_map: &git_map,
                git_cache,
                host_id,
                cache,
                now,
            };
            let sessions = sessions_from_panes(&mut build, &panes);
            cache.update_host(host_id, "ok", now);
            Ok(HostSnapshot {
                host: host_id.to_string(),
                tmux_socket: host_config.tmux_socket().map(str::to_string),
                status: SnapshotStatus::Ok,
                collected_at: now,
                sessions,
                errors: Vec::new(),
            })
        }
        Err(err) => {
            cache.update_host(host_id, "unreachable", now);
            let message = format!("{err:#}");
            Ok(HostSnapshot {
                host: host_id.to_string(),
                tmux_socket: host_config.tmux_socket().map(str::to_string),
                status: SnapshotStatus::Unreachable,
                collected_at: now,
                sessions: unreachable_sessions(config, &host_config, &message),
                errors: vec![SnapshotError {
                    kind: "poll".to_string(),
                    message,
                }],
            })
        }
    }
}

fn sessions_from_panes(ctx: &mut SnapshotBuildContext<'_>, panes: &[Pane]) -> Vec<SessionSnapshot> {
    let watches = ctx.config.watches_for_host(&ctx.host_config.id);
    let (watch_resolutions, claimed_targets) = resolve_watch_matches(&watches, panes);
    let mut snapshots = Vec::new();

    for resolution in watch_resolutions {
        match resolution.status {
            MatchStatus::Matched | MatchStatus::Shadowed => {
                let pane = resolution
                    .pane_index
                    .and_then(|index| panes.get(index))
                    .expect("watch resolution pane index should be valid");
                snapshots.push(snapshot_for_pane(
                    ctx,
                    pane,
                    PaneRowContext {
                        watch: Some(&resolution.watch),
                        match_status: resolution.status,
                        candidate_targets: resolution.candidate_targets,
                        shadowed_by: resolution.shadowed_by,
                    },
                ));
            }
            MatchStatus::Missing | MatchStatus::Ambiguous => {
                snapshots.push(watch_without_pane_snapshot(
                    ctx.host_config,
                    &resolution.watch,
                    resolution.status,
                    resolution.candidate_targets,
                    resolution.shadowed_by,
                    None,
                ));
            }
            MatchStatus::Orphan | MatchStatus::Unreachable | MatchStatus::Unknown => {}
        }
    }

    for pane in panes {
        if !claimed_targets.contains(&pane.target) {
            snapshots.push(snapshot_for_pane(
                ctx,
                pane,
                PaneRowContext {
                    watch: None,
                    match_status: MatchStatus::Orphan,
                    candidate_targets: Vec::new(),
                    shadowed_by: None,
                },
            ));
        }
    }

    snapshots
}

fn resolve_watch_matches(
    watches: &[IndexedWatch],
    panes: &[Pane],
) -> (Vec<WatchResolution>, HashSet<String>) {
    let mut claimed_by: HashMap<String, String> = HashMap::new();
    let mut resolutions = Vec::new();

    for watch in watches {
        let candidates: Vec<usize> = panes
            .iter()
            .enumerate()
            .filter_map(|(index, pane)| watch_matches_pane(&watch.watch, pane).then_some(index))
            .collect();
        let candidate_targets = candidates
            .iter()
            .map(|index| panes[*index].target.clone())
            .collect();

        match candidates.as_slice() {
            [] => resolutions.push(WatchResolution {
                watch: watch.clone(),
                status: MatchStatus::Missing,
                pane_index: None,
                candidate_targets,
                shadowed_by: None,
            }),
            [pane_index] => {
                let pane = &panes[*pane_index];
                if let Some(winner) = claimed_by.get(&pane.target) {
                    resolutions.push(WatchResolution {
                        watch: watch.clone(),
                        status: MatchStatus::Shadowed,
                        pane_index: Some(*pane_index),
                        candidate_targets,
                        shadowed_by: Some(winner.clone()),
                    });
                } else {
                    claimed_by.insert(pane.target.clone(), watch.watch.id.clone());
                    resolutions.push(WatchResolution {
                        watch: watch.clone(),
                        status: MatchStatus::Matched,
                        pane_index: Some(*pane_index),
                        candidate_targets: Vec::new(),
                        shadowed_by: None,
                    });
                }
            }
            _ => resolutions.push(WatchResolution {
                watch: watch.clone(),
                status: MatchStatus::Ambiguous,
                pane_index: None,
                candidate_targets,
                shadowed_by: None,
            }),
        }
    }

    (resolutions, claimed_by.into_keys().collect())
}

fn watch_matches_pane(watch: &Watch, pane: &Pane) -> bool {
    let matcher = &watch.matcher;
    if let Some(command) = &matcher.command
        && pane.command != *command
    {
        return false;
    }
    if let Some(cwd) = &matcher.cwd
        && pane.cwd != *cwd
    {
        return false;
    }
    if let Some(prefix) = &matcher.cwd_prefix
        && !path_matches_prefix(&pane.cwd, prefix)
    {
        return false;
    }
    if let Some(tmux) = &matcher.tmux
        && !tmux::matches_target(pane, tmux)
    {
        return false;
    }
    true
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return path.starts_with('/');
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn snapshot_for_pane(
    ctx: &mut SnapshotBuildContext<'_>,
    pane: &Pane,
    row: PaneRowContext<'_>,
) -> SessionSnapshot {
    let display_id = row
        .watch
        .map(|watch| watch.watch.id.clone())
        .unwrap_or_else(|| pane.target.clone());
    let cache_key = format!("{}/{}", ctx.host_config.id, display_id);
    let (state, output, errors) = match ctx.captures.get(&pane.pane_id) {
        Some(Some(output)) => {
            let output_hash = hash(output);
            let (state, last_output_at) = ctx.cache.update_output(
                &cache_key,
                Some(pane.pane_id.clone()),
                &output_hash,
                ctx.now,
                &ctx.config.poll,
            );
            (
                state,
                Some(OutputSnapshot {
                    preview: preview(output),
                    recent: recent_preview(output, 12),
                    hash: output_hash,
                    last_output_at,
                }),
                Vec::new(),
            )
        }
        Some(None) => (
            SessionState::Unknown,
            None,
            vec![SnapshotError {
                kind: "capture".to_string(),
                message: format!("capture-pane failed for pane {}", pane.pane_id),
            }],
        ),
        None => (
            SessionState::Unknown,
            None,
            vec![SnapshotError {
                kind: "capture".to_string(),
                message: format!("no capture data for pane {}", pane.pane_id),
            }],
        ),
    };

    let repo = if let Some(configured_repo) = row.watch.and_then(|w| w.watch.repo.as_deref()) {
        let cache_key = (ctx.host_id.to_string(), configured_repo.to_string());
        let cached = git_cache::get_fresh(ctx.git_cache, &cache_key, ctx.config.poll.git_cache_ttl);
        if let Some(snap) = cached {
            snap
        } else {
            let snap = Some(git::collect(ctx.config, ctx.host_config, configured_repo));
            git_cache::insert(ctx.git_cache, cache_key, snap.clone());
            snap
        }
    } else {
        ctx.git_map.get(&pane.pane_id).cloned().unwrap_or_else(|| {
            git_cache::get_fresh(
                ctx.git_cache,
                &(ctx.host_id.to_string(), pane.cwd.clone()),
                ctx.config.poll.git_cache_ttl,
            )
            .unwrap_or(None)
        })
    };

    let detected_agent = infer_coding_agent(
        &pane.command,
        pane.window_name.as_deref(),
        pane.pane_title.as_deref(),
    );

    SessionSnapshot {
        session_id: display_id.clone(),
        target: pane.target.clone(),
        display_id,
        raw_target: Some(pane.target.clone()),
        host: ctx.host_config.id.clone(),
        tmux_socket: ctx.host_config.tmux_socket().map(str::to_string),
        match_status: row.match_status,
        watch_id: row.watch.map(|watch| watch.watch.id.clone()),
        watch_index: row.watch.map(|watch| watch.index),
        candidate_targets: row.candidate_targets,
        shadowed_by: row.shadowed_by,
        state,
        agent_hint: row
            .watch
            .and_then(|watch| watch.watch.agent_hint.clone())
            .or_else(|| detected_agent.map(str::to_string)),
        tmux: TmuxSnapshot {
            session: pane.session.clone(),
            window: Some(pane.window.clone()),
            pane: Some(pane.pane.clone()),
            pane_id: Some(pane.pane_id.clone()),
            session_attached: Some(pane.session_attached),
            window_name: pane.window_name.clone(),
            pane_title: pane.pane_title.clone(),
            host_short: pane.host_short.clone(),
        },
        process: Some(ProcessSnapshot {
            pid: pane.pid,
            command: pane.command.clone(),
            cwd: pane.cwd.clone(),
        }),
        repo,
        output,
        errors,
    }
}

fn infer_coding_agent(
    command: &str,
    window_name: Option<&str>,
    pane_title: Option<&str>,
) -> Option<&'static str> {
    if let Some(agent) = agent_from_command(command) {
        return Some(agent);
    }

    agent_from_labels(window_name, pane_title)
}

fn agent_from_labels(window_name: Option<&str>, pane_title: Option<&str>) -> Option<&'static str> {
    for label in [window_name, pane_title].into_iter().flatten() {
        if is_pi_agent_label(label) {
            return Some("pi");
        }
        if label.to_ascii_lowercase().contains("claude code") {
            return Some("claude");
        }
    }

    None
}

fn agent_from_command(command: &str) -> Option<&'static str> {
    let command = command
        .rsplit('/')
        .next()
        .unwrap_or(command)
        .trim()
        .to_ascii_lowercase();
    match command.as_str() {
        "pi" | "pi-cli" => Some("pi"),
        "claude" | "claude-code" => Some("claude"),
        "codex" | "codex-cli" => Some("codex"),
        "kiro" | "kiro-cli" => Some("kiro"),
        _ => None,
    }
}

fn is_pi_agent_label(label: &str) -> bool {
    let label = label.trim_start();
    if let Some(first) = label.chars().next()
        && matches!(first, '\u{03c0}' | '\u{03a0}')
    {
        return true;
    }

    let label = label.to_ascii_lowercase();
    matches!(
        label.strip_prefix("pi"),
        Some(rest)
            if rest.starts_with(" - ")
                || rest.starts_with(" | ")
                || rest.starts_with(": ")
                || rest == " coding agent"
    )
}

fn watch_without_pane_snapshot(
    host_config: &HostConfig,
    watch: &IndexedWatch,
    match_status: MatchStatus,
    candidate_targets: Vec<String>,
    shadowed_by: Option<String>,
    message: Option<String>,
) -> SessionSnapshot {
    let target = watch_target_string(host_config, &watch.watch);
    let state = match match_status {
        MatchStatus::Missing => SessionState::Missing,
        MatchStatus::Unreachable => SessionState::Unreachable,
        _ => SessionState::Unknown,
    };
    let message = message.unwrap_or_else(|| match match_status {
        MatchStatus::Missing => format!("watch `{}` did not match a live pane", watch.watch.id),
        MatchStatus::Ambiguous => format!(
            "watch `{}` matched {} live panes",
            watch.watch.id,
            candidate_targets.len()
        ),
        MatchStatus::Shadowed => format!(
            "watch `{}` matched a pane claimed by `{}`",
            watch.watch.id,
            shadowed_by.as_deref().unwrap_or("an earlier watch")
        ),
        MatchStatus::Unreachable => format!("host `{}` was unreachable", host_config.id),
        _ => format!(
            "watch `{}` has status {}",
            watch.watch.id,
            match_status.as_str()
        ),
    });

    SessionSnapshot {
        session_id: watch.watch.id.clone(),
        target,
        display_id: watch.watch.id.clone(),
        raw_target: None,
        host: host_config.id.clone(),
        tmux_socket: host_config.tmux_socket().map(str::to_string),
        match_status,
        watch_id: Some(watch.watch.id.clone()),
        watch_index: Some(watch.index),
        candidate_targets,
        shadowed_by,
        state,
        agent_hint: watch.watch.agent_hint.clone(),
        tmux: TmuxSnapshot {
            session: watch
                .watch
                .matcher
                .tmux
                .as_ref()
                .map(|tmux| tmux.session.clone())
                .unwrap_or_else(|| "-".to_string()),
            window: watch
                .watch
                .matcher
                .tmux
                .as_ref()
                .and_then(|tmux| tmux.window.map(|window| window.to_string())),
            pane: watch
                .watch
                .matcher
                .tmux
                .as_ref()
                .and_then(|tmux| tmux.pane.map(|pane| pane.to_string())),
            pane_id: None,
            session_attached: None,
            window_name: None,
            pane_title: None,
            host_short: None,
        },
        process: None,
        repo: None,
        output: None,
        errors: vec![SnapshotError {
            kind: match_status.as_str().to_string(),
            message,
        }],
    }
}

fn unreachable_sessions(config: &Config, host: &HostConfig, message: &str) -> Vec<SessionSnapshot> {
    config
        .watches_for_host(&host.id)
        .into_iter()
        .map(|watch| {
            watch_without_pane_snapshot(
                host,
                &watch,
                MatchStatus::Unreachable,
                Vec::new(),
                None,
                Some(message.to_string()),
            )
        })
        .collect()
}

fn target_for_watch(config: &Config, watch_id: &str, action: &str) -> Result<PaneTarget> {
    let watch = config.watch(watch_id)?;
    let host_snapshot = snapshot_host_with_persistent_cache(config, &watch.watch.host)?;
    let snapshot = host_snapshot
        .sessions
        .into_iter()
        .find(|snapshot| snapshot.watch_id.as_deref() == Some(watch_id))
        .ok_or_else(|| anyhow!("watch `{watch_id}` was not found in snapshot"))?;

    match snapshot.match_status {
        MatchStatus::Matched => target_for_snapshot(&snapshot),
        MatchStatus::Ambiguous => bail!(
            "cannot {action} watch `{watch_id}` because it is ambiguous; candidates: {}",
            snapshot.candidate_targets.join(", ")
        ),
        MatchStatus::Missing => {
            bail!("cannot {action} watch `{watch_id}` because no live pane matched")
        }
        MatchStatus::Shadowed => bail!(
            "cannot {action} watch `{watch_id}` because it is shadowed by `{}`",
            snapshot
                .shadowed_by
                .as_deref()
                .unwrap_or("an earlier watch")
        ),
        MatchStatus::Unreachable => bail!(
            "cannot {action} watch `{watch_id}` because host `{}` is unreachable",
            snapshot.host
        ),
        MatchStatus::Orphan | MatchStatus::Unknown => bail!(
            "cannot {action} watch `{watch_id}` with status {}",
            snapshot.match_status.as_str()
        ),
    }
}

fn target_for_snapshot(snapshot: &SessionSnapshot) -> Result<PaneTarget> {
    Ok(PaneTarget {
        host: snapshot.host.clone(),
        session: snapshot.tmux.session.clone(),
        window: snapshot.tmux.window.clone().ok_or_else(|| {
            anyhow!(
                "session `{}` has no resolved tmux window",
                snapshot.display_id
            )
        })?,
        pane: snapshot.tmux.pane.clone().ok_or_else(|| {
            anyhow!(
                "session `{}` has no resolved tmux pane",
                snapshot.display_id
            )
        })?,
        pane_id: snapshot.tmux.pane_id.clone(),
    })
}

fn watch_target_string(host_config: &HostConfig, watch: &Watch) -> String {
    let Some(tmux) = &watch.matcher.tmux else {
        return "-".to_string();
    };
    match (tmux.window, tmux.pane) {
        (Some(window), Some(pane)) => {
            format!("{}/{}:{}.{}", host_config.id, tmux.session, window, pane)
        }
        (Some(window), None) => format!("{}/{}:{}.*", host_config.id, tmux.session, window),
        _ => format!("{}/{}", host_config.id, tmux.session),
    }
}

fn preview(output: &str) -> String {
    output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

fn recent_preview(output: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn hash(output: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(output.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn capture_for_inspect(
    config: &Config,
    target: &PaneTarget,
    snapshot: &mut SessionSnapshot,
    color: bool,
) -> Result<String> {
    match capture_pane(config, target, config.poll.capture_lines, color) {
        Ok(output) => Ok(output),
        Err(err) => {
            let message = format!("{err:#}");
            if !snapshot
                .errors
                .iter()
                .any(|error| error.kind == "capture" && error.message == message)
            {
                snapshot.errors.push(SnapshotError {
                    kind: "capture".to_string(),
                    message,
                });
            }
            Ok(String::new())
        }
    }
}

fn append_cache_warning(snapshot: &mut HostSnapshot, warning: Option<String>) {
    if let Some(message) = warning {
        snapshot.errors.push(SnapshotError {
            kind: "cache".to_string(),
            message,
        });
    }
}

fn append_cache_save_error(snapshot: &mut HostSnapshot, error: Option<anyhow::Error>) {
    if let Some(error) = error {
        snapshot.errors.push(SnapshotError {
            kind: "cache".to_string(),
            message: format!("{error:#}"),
        });
    }
}

#[cfg(test)]
mod tests;
