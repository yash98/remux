use crate::cache::Cache;
use crate::config::{Config, HostKind};
use crate::sessions::SessionRollup;
use crate::snapshot::{HostSnapshot, PaneDetail, SessionSnapshot, SnapshotStatus};
use anyhow::Result;
use chrono::{DateTime, Utc};

pub fn hosts(config: &Config) -> Result<()> {
    let cache_load = Cache::load_with_warning();
    if let Some(warning) = &cache_load.warning {
        eprintln!("warning: cache: {warning}");
    }
    let cache = cache_load.cache;

    let headers = &["HOST", "TYPE", "TARGET", "SOCKET", "STATUS", "LAST POLL"];
    let rows: Vec<Vec<String>> = config
        .hosts
        .iter()
        .map(|host| {
            let raw_target = match host.kind {
                HostKind::Local => "-".to_string(),
                HostKind::Ssh | HostKind::Docker => host
                    .ssh()
                    .ok()
                    .and_then(|ssh| ssh.target())
                    .unwrap_or_else(|| "-".to_string()),
            };
            let target = clamp_preview(&raw_target, 30);
            let kind = match host.kind {
                HostKind::Local => "local",
                HostKind::Ssh => "ssh",
                HostKind::Docker => "docker",
            };
            let socket = host
                .tmux_socket()
                .map(|socket| clamp_preview(socket, 30))
                .unwrap_or_else(|| "-".to_string());
            let (status, last_poll) = cache
                .hosts
                .get(&host.id)
                .map(|entry| {
                    (
                        entry.status.as_str().to_string(),
                        entry.last_poll_at.to_rfc3339(),
                    )
                })
                .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
            vec![
                host.id.clone(),
                kind.to_string(),
                target,
                socket,
                status,
                last_poll,
            ]
        })
        .collect();
    print!("{}", render_table(headers, rows, None));
    Ok(())
}

pub fn snapshot(snapshot: &HostSnapshot, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(snapshot)?);
        return Ok(());
    }

    match snapshot.status {
        SnapshotStatus::Ok => {
            println!("Host: {}", snapshot.host);
            if let Some(socket) = &snapshot.tmux_socket {
                println!("Socket: {socket}");
            }
            println!("Collected: {}", snapshot.collected_at);
            print_snapshot_errors(snapshot);
            println!();
            print_session_table(&snapshot.sessions);
        }
        SnapshotStatus::Unreachable => {
            println!("Host: {}", snapshot.host);
            if let Some(socket) = &snapshot.tmux_socket {
                println!("Socket: {socket}");
            }
            println!("Status: unreachable");
            print_snapshot_errors(snapshot);
            if !snapshot.sessions.is_empty() {
                println!();
                print_session_table(&snapshot.sessions);
            }
        }
    }

    Ok(())
}

pub fn list(snapshots: &[HostSnapshot], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(snapshots)?);
        return Ok(());
    }

    let headers = &[
        "TARGET", "MATCH", "PANE", "CMD", "REPO", "STATE", "DIRTY", "PREVIEW",
    ];
    let mut rows: Vec<Vec<String>> = Vec::new();
    for snapshot in snapshots {
        for session in &snapshot.sessions {
            let pane = session
                .tmux
                .window
                .as_ref()
                .zip(session.tmux.pane.as_ref())
                .map(|(window, pane)| format!("{window}.{pane}"))
                .unwrap_or_else(|| "-".to_string());
            let command = session.display_command().unwrap_or_else(|| "-".to_string());
            let repo = session
                .repo
                .as_ref()
                .map(|repo| basename(repo.path.as_str()).to_string())
                .unwrap_or_else(|| "-".to_string());
            let dirty = session
                .repo
                .as_ref()
                .and_then(|repo| repo.dirty_count)
                .map(|dirty| dirty.to_string())
                .unwrap_or_else(|| "-".to_string());
            let raw_preview = session
                .output
                .as_ref()
                .map(|output| output.preview.as_str())
                .unwrap_or_else(|| first_error_message(session).unwrap_or("-"));
            let preview = clamp_preview(raw_preview, 60);
            rows.push(vec![
                session.display_id.clone(),
                session.match_status.as_str().to_string(),
                pane,
                command,
                repo,
                session.state.as_str().to_string(),
                dirty,
                preview,
            ]);
        }
    }
    print!("{}", render_table(headers, rows, None));
    warn_snapshot_errors(snapshots);
    Ok(())
}

pub fn sessions(sessions: &[SessionRollup], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(sessions)?);
        return Ok(());
    }

    let headers = &[
        "HOST",
        "SESSION",
        "A",
        "WINDOWS",
        "PANES",
        "STATE",
        "ACTIVE CMD",
    ];
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|session| {
            vec![
                session.host.clone(),
                session.session.clone(),
                if session.attached {
                    "*".to_string()
                } else {
                    String::new()
                },
                session.windows.to_string(),
                session.panes.to_string(),
                session.state.as_str().to_string(),
                session.active_cmd.as_deref().unwrap_or("-").to_string(),
            ]
        })
        .collect();
    print!("{}", render_table(headers, rows, None));
    Ok(())
}

pub fn inspect(detail: &PaneDetail, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(detail)?);
        return Ok(());
    }

    let session = &detail.session;
    // Collect label/value pairs so we can align colons.
    let mut pairs: Vec<(&str, String)> = vec![
        ("Session", session.display_id.clone()),
        (
            "Target",
            session.raw_target.as_deref().unwrap_or("-").to_string(),
        ),
        ("Host", session.host.clone()),
        ("Match", session.match_status.as_str().to_string()),
    ];
    if let Some(watch_id) = &session.watch_id {
        pairs.push(("Watch", watch_id.clone()));
    }
    pairs.push((
        "Tmux socket",
        session
            .tmux_socket
            .as_deref()
            .unwrap_or("default")
            .to_string(),
    ));
    if let Some(shadowed_by) = &session.shadowed_by {
        pairs.push(("Shadowed by", shadowed_by.clone()));
    }
    pairs.push(("State", session.state.as_str().to_string()));
    pairs.push((
        "Tmux target",
        format!(
            "{}:{}",
            session.tmux.session,
            session
                .tmux
                .window
                .as_ref()
                .zip(session.tmux.pane.as_ref())
                .map(|(window, pane)| format!("{window}.{pane}"))
                .unwrap_or_else(|| "-".to_string())
        ),
    ));
    pairs.push((
        "Pane ID",
        session.tmux.pane_id.as_deref().unwrap_or("-").to_string(),
    ));
    pairs.push((
        "Agent hint",
        session.agent_hint.as_deref().unwrap_or("-").to_string(),
    ));
    if let Some(process) = &session.process {
        pairs.push((
            "PID",
            process
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ));
        pairs.push(("Command", process.command.clone()));
        pairs.push(("CWD", process.cwd.clone()));
    }
    if let Some(repo) = &session.repo {
        pairs.push(("Repo", repo.path.clone()));
        pairs.push(("Branch", repo.branch.as_deref().unwrap_or("-").to_string()));
        pairs.push((
            "Dirty files",
            repo.dirty_count
                .map(|dirty| dirty.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ));
        if let Some(error) = &repo.error {
            pairs.push(("Repo error", error.clone()));
        }
    }
    if let Some(output) = &session.output {
        let age = output
            .last_output_at
            .map(format_age)
            .unwrap_or_else(|| "-".to_string());
        pairs.push(("Last output", age));
    }

    let label_width = pairs
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(0);
    for (label, value) in &pairs {
        println!("{:<width$}  {}", label, value, width = label_width);
    }

    for error in &session.errors {
        println!("{} error: {}", titlecase(&error.kind), error.message);
    }
    if !session.candidate_targets.is_empty() {
        println!("Candidates:");
        for target in &session.candidate_targets {
            println!("  {target}");
        }
    }
    println!();
    println!("Recent output:");
    println!("{}", detail.recent_output);
    if let Some(repo) = &session.repo
        && !repo.changed_files.is_empty()
    {
        println!("Changed files:");
        for file in &repo.changed_files {
            println!("  {file}");
        }
    }
    println!("Commands:");
    if session.match_status == crate::snapshot::MatchStatus::Matched {
        println!("  remux capture {} --lines 200", session.display_id);
        println!("  remux attach --readonly {}", session.display_id);
        println!("  remux attach {}", session.display_id);
    } else if let Some(raw_target) = &session.raw_target {
        println!("  remux capture '{}' --lines 200", raw_target);
        println!("  remux attach --readonly '{}'", raw_target);
        println!("  remux attach '{}'", raw_target);
    }

    Ok(())
}

/// Render captured pane output with a header showing target, line count, and age.
/// If `color` is false, ANSI escapes are stripped from the content.
pub fn capture_output(
    target: &str,
    content: &str,
    last_output_at: Option<DateTime<Utc>>,
    color: bool,
) {
    let line_count = content.lines().count();
    let age_part = last_output_at
        .map(|ts| format!(", {}", format_age(ts)))
        .unwrap_or_default();
    println!("Captured: {target} ({line_count} lines{age_part})");
    println!("{}", "-".repeat(40));
    if color {
        print!("{content}");
    } else {
        print!("{}", strip_ansi(content));
    }
}

fn print_snapshot_errors(snapshot: &HostSnapshot) {
    for error in &snapshot.errors {
        println!("{}: {}", error.kind, error.message);
    }
}

pub fn warn_snapshot_errors(snapshots: &[HostSnapshot]) {
    for snapshot in snapshots {
        for error in &snapshot.errors {
            eprintln!(
                "warning: {}: {}: {}",
                snapshot.host, error.kind, error.message
            );
        }
    }
}

fn print_session_table(sessions: &[SessionSnapshot]) {
    let headers = &[
        "SESSION", "MATCH", "TARGET", "PANE_ID", "CMD", "STATE", "CWD", "PREVIEW",
    ];
    let rows: Vec<Vec<String>> = sessions
        .iter()
        .map(|session| {
            let command = session.display_command().unwrap_or_else(|| "-".to_string());
            let cwd = session
                .process
                .as_ref()
                .map(|process| process.cwd.clone())
                .unwrap_or_else(|| "-".to_string());
            let raw_preview = session
                .output
                .as_ref()
                .map(|output| output.preview.as_str())
                .unwrap_or("");
            let preview = clamp_preview(raw_preview, 80);
            vec![
                session.display_id.clone(),
                session.match_status.as_str().to_string(),
                session.raw_target.as_deref().unwrap_or("-").to_string(),
                session.tmux.pane_id.as_deref().unwrap_or("-").to_string(),
                command,
                session.state.as_str().to_string(),
                cwd,
                preview,
            ]
        })
        .collect();
    print!("{}", render_table(headers, rows, None));
    // Print per-session extras (candidates, errors) after the table.
    for session in sessions {
        if !session.candidate_targets.is_empty() {
            println!(
                "  {} candidates: {}",
                session.display_id,
                session.candidate_targets.join(", ")
            );
        }
        for error in &session.errors {
            println!("  {} {}: {}", session.display_id, error.kind, error.message);
        }
    }
}

fn titlecase(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(path)
}

fn first_error_message(session: &SessionSnapshot) -> Option<&str> {
    session.errors.first().map(|error| error.message.as_str())
}

/// Format a UTC timestamp as a human-readable relative age: "5s ago", "3m ago", "2h ago".
fn format_age(ts: DateTime<Utc>) -> String {
    let secs = (Utc::now() - ts).num_seconds().max(0) as u64;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

// ---------------------------------------------------------------------------
// Shared text helpers
// ---------------------------------------------------------------------------

/// Remove ANSI CSI escape sequences (ESC `[` … final-byte) from `s`.
#[allow(dead_code)]
pub fn strip_ansi(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            // consume until a byte in 0x40–0x7E (the CSI final byte)
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1; // skip final byte
        } else {
            let Some(ch) = s[i..].chars().next() else {
                break;
            };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Produce a clean single-line preview string no wider than `max_width`.
/// Steps: strip ANSI → collapse whitespace → trim → first line → truncate.
#[allow(dead_code)]
pub fn clamp_preview(s: &str, max_width: usize) -> String {
    let stripped = strip_ansi(s);
    // collapse runs of spaces/tabs to a single space, drop newlines
    let mut collapsed = String::with_capacity(stripped.len());
    let mut prev_space = false;
    for ch in stripped.chars() {
        if ch == '\n' || ch == '\r' {
            break; // first line only
        } else if ch == ' ' || ch == '\t' {
            if !prev_space {
                collapsed.push(' ');
            }
            prev_space = true;
        } else {
            collapsed.push(ch);
            prev_space = false;
        }
    }
    let trimmed = collapsed.trim().to_string();
    if max_width == 0 || trimmed.chars().count() <= max_width {
        return trimmed;
    }
    // truncate to max_width-1 chars then append ellipsis (1 char)
    let truncated: String = trimmed.chars().take(max_width - 1).collect();
    format!("{truncated}…")
}

/// Return per-column max widths across headers and all rows.
/// Handles ragged rows by using the max column count seen.
/// If `max_widths` is provided and non-zero for a column, cap that column at that value.
/// Columns with no explicit cap default to 60.
#[allow(dead_code)]
pub fn compute_col_widths(
    headers: &[&str],
    rows: &[Vec<String>],
    max_widths: Option<&[usize]>,
) -> Vec<usize> {
    let ncols = rows
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(0)
        .max(headers.len());
    let mut widths: Vec<usize> = (0..ncols)
        .map(|i| headers.get(i).map_or(0, |h| h.len()))
        .collect();
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            if cell.len() > widths[col] {
                widths[col] = cell.len();
            }
        }
    }
    for (col, w) in widths.iter_mut().enumerate() {
        let cap = max_widths
            .and_then(|caps| caps.get(col).copied())
            .filter(|&c| c > 0)
            .unwrap_or(60);
        if *w > cap {
            *w = cap;
        }
    }
    widths
}

/// Render a plaintext table with 2-space gutters.  Returns the full string
/// (caller can print it).  Cells are already formatted by the caller.
#[allow(dead_code)]
pub fn render_table(
    headers: &[&str],
    rows: Vec<Vec<String>>,
    max_col_widths: Option<&[usize]>,
) -> String {
    let widths = compute_col_widths(headers, &rows, max_col_widths);
    let mut out = String::new();
    // header row
    for (col, header) in headers.iter().enumerate() {
        if col > 0 {
            out.push_str("  ");
        }
        if col + 1 < headers.len() {
            out.push_str(&format!("{:<width$}", header, width = widths[col]));
        } else {
            out.push_str(header); // last column: no padding
        }
    }
    out.push('\n');
    // data rows
    for row in &rows {
        for (col, cell) in row.iter().enumerate().take(headers.len()) {
            if col > 0 {
                out.push_str("  ");
            }
            if col + 1 < headers.len() {
                out.push_str(&format!("{:<width$}", cell, width = widths[col]));
            } else {
                out.push_str(cell);
            }
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- strip_ansi ---

    #[test]
    fn strip_ansi_empty() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_plain() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_color_codes() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
    }

    #[test]
    fn strip_ansi_bold_and_reset() {
        assert_eq!(
            strip_ansi("\x1b[1;32mbold green\x1b[0m text"),
            "bold green text"
        );
    }

    #[test]
    fn strip_ansi_no_trailing_garbage() {
        assert_eq!(strip_ansi("a\x1b[mb"), "ab");
    }

    // --- clamp_preview ---

    #[test]
    fn clamp_preview_empty() {
        assert_eq!(clamp_preview("", 20), "");
    }

    #[test]
    fn clamp_preview_plain_short() {
        assert_eq!(clamp_preview("hello", 20), "hello");
    }

    #[test]
    fn clamp_preview_strips_ansi() {
        assert_eq!(clamp_preview("\x1b[31mred\x1b[0m", 20), "red");
    }

    #[test]
    fn clamp_preview_collapses_tabs_and_spaces() {
        assert_eq!(clamp_preview("a\t\t  b", 20), "a b");
    }

    #[test]
    fn clamp_preview_first_line_only() {
        assert_eq!(clamp_preview("line1\nline2", 20), "line1");
    }

    #[test]
    fn clamp_preview_truncates() {
        // max_width=5: 4 chars + ellipsis
        assert_eq!(clamp_preview("abcdefgh", 5), "abcd…");
    }

    #[test]
    fn clamp_preview_exact_width() {
        assert_eq!(clamp_preview("abcde", 5), "abcde");
    }

    #[test]
    fn clamp_preview_max_width_zero() {
        // zero means no truncation
        assert_eq!(clamp_preview("anything", 0), "anything");
    }

    // --- compute_col_widths ---

    #[test]
    fn col_widths_empty_rows() {
        let widths = compute_col_widths(&["HOST", "STATE"], &[], None);
        assert_eq!(widths, vec![4, 5]);
    }

    #[test]
    fn col_widths_header_wider() {
        let rows = vec![vec!["a".to_string(), "b".to_string()]];
        let widths = compute_col_widths(&["HOSTNAME", "ST"], &rows, None);
        assert_eq!(widths, vec![8, 2]);
    }

    #[test]
    fn col_widths_row_wider() {
        let rows = vec![vec!["very-long-value".to_string(), "x".to_string()]];
        let widths = compute_col_widths(&["H", "S"], &rows, None);
        assert_eq!(widths, vec![15, 1]);
    }

    #[test]
    fn col_widths_capped() {
        let rows = vec![vec!["a".repeat(80), "b".to_string()]];
        let widths = compute_col_widths(&["H", "S"], &rows, Some(&[10, 60]));
        assert_eq!(widths, vec![10, 1]);
    }

    #[test]
    fn col_widths_default_cap_60() {
        let rows = vec![vec!["x".repeat(100)]];
        let widths = compute_col_widths(&["H"], &rows, None);
        assert_eq!(widths, vec![60]);
    }

    #[test]
    fn col_widths_zero_cap_means_no_cap() {
        // A zero entry in max_widths means "no explicit cap" — falls back to default 60.
        // Value is 100 chars, so it gets capped at 60.
        let rows = vec![vec!["x".repeat(100)]];
        let widths = compute_col_widths(&["H"], &rows, Some(&[0]));
        assert_eq!(widths, vec![60]);
    }

    #[test]
    fn col_widths_ragged_rows() {
        // Row has more columns than headers — extra column should be tracked.
        let rows = vec![vec!["a".to_string(), "bb".to_string(), "ccc".to_string()]];
        let widths = compute_col_widths(&["H"], &rows, None);
        assert_eq!(widths.len(), 3);
        assert_eq!(widths[2], 3);
    }

    // --- render_table ---

    #[test]
    fn render_table_basic() {
        let headers = &["NAME", "STATE"];
        let rows = vec![
            vec!["alpha".to_string(), "ok".to_string()],
            vec!["beta".to_string(), "err".to_string()],
        ];
        let out = render_table(headers, rows, None);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("NAME"));
        assert!(lines[1].contains("alpha"));
        assert!(lines[2].contains("beta"));
    }

    #[test]
    fn render_table_empty_rows() {
        let out = render_table(&["A", "B"], vec![], None);
        assert_eq!(out.lines().count(), 1); // header only
    }

    #[test]
    fn render_table_header_wider_than_rows() {
        let headers = &["LONG_HEADER", "X"];
        let rows = vec![vec!["a".to_string(), "b".to_string()]];
        let out = render_table(headers, rows, None);
        // first data cell should be padded to header width
        let data_line = out.lines().nth(1).unwrap();
        assert!(data_line.starts_with("a          ")); // 11 chars padded
    }

    #[test]
    fn render_table_row_wider_than_header() {
        let headers = &["H", "S"];
        let rows = vec![vec!["long-value".to_string(), "x".to_string()]];
        let out = render_table(headers, rows, None);
        let header_line = out.lines().next().unwrap();
        // header H should be padded to 10 chars
        assert!(header_line.starts_with("H         "));
    }
}
