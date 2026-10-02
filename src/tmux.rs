use crate::config::TmuxTarget;
use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;

// Several inventory fields are free-form and user/app-controlled — session
// names, window names, pane titles, and the current command/path can all
// contain literal tabs or newlines that would split or stretch a tab-
// separated row. We use ASCII Unit Separator (\x1f, "INVENTORY_FIELD_SEP")
// as the field delimiter so any tab or newline a user/app might inject
// into a tmux variable stays inside its own field instead of corrupting
// the row layout.
pub const INVENTORY_FIELD_SEP: char = '\x1f';
const INVENTORY_FIELD_SEP_ESCAPED: &str = "\\037";
// Field separator: literal `\037` text. tmux passes it through untouched
// (both directly and inside containers, where tmux mangles real control
// characters to `_`), and INVENTORY_FIELD_SEP_ESCAPED parses it.
pub const INVENTORY_FORMAT: &str = "'#S\\037#I\\037#P\\037#{pane_id}\\037#{pane_pid}\\037#{pane_current_command}\\037#{pane_current_path}\\037#{session_attached}\\037#W\\037#{pane_title}\\037#{host_short}'";

pub type PaneCaptures = HashMap<String, Option<String>>;
pub type PaneGitSnapshots = HashMap<String, Option<crate::git::RepoSnapshot>>;
pub type InventoryWithCaptures = (Vec<Pane>, PaneCaptures, PaneGitSnapshots);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneTarget {
    pub host: String,
    pub session: String,
    pub window: String,
    pub pane: String,
    pub pane_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pane {
    pub target: String,
    pub host: String,
    pub session: String,
    pub window: String,
    pub pane: String,
    pub pane_id: String,
    pub pid: Option<u32>,
    pub command: String,
    pub cwd: String,
    pub session_attached: bool,
    pub window_name: Option<String>,
    pub pane_title: Option<String>,
    pub host_short: Option<String>,
}

impl PaneTarget {
    pub fn parse(input: &str) -> Result<Self> {
        let (host, rest) = input.split_once('/').ok_or_else(|| {
            anyhow!("pane target must look like <host>/<session>:<window>.<pane>")
        })?;
        let (session, rest) = rest
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("pane target must include tmux session and window"))?;
        let (window, pane) = rest.split_once('.').ok_or_else(|| {
            anyhow!("pane target must include window and pane as <window>.<pane>")
        })?;

        if host.is_empty() || session.is_empty() || window.is_empty() || pane.is_empty() {
            bail!("pane target must look like <host>/<session>:<window>.<pane>");
        }

        Ok(Self {
            host: host.to_string(),
            session: session.to_string(),
            window: window.to_string(),
            pane: pane.to_string(),
            pane_id: None,
        })
    }

    pub fn tmux_target(&self) -> String {
        format!("{}:{}.{}", self.session, self.window, self.pane)
    }
}

impl fmt::Display for PaneTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}:{}.{}",
            self.host, self.session, self.window, self.pane
        )
    }
}

pub fn parse_inventory(host: &str, output: &str) -> Result<Vec<Pane>> {
    let mut panes = Vec::new();
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // Accept both the new \x1f-separated layout (used by INVENTORY_FORMAT
        // so user-controlled fields can safely contain tabs) and legacy
        // 8-field tab-separated rows from older tmux output captured in
        // tests or pre-upgrade fixtures.
        let fields: Vec<&str> = if line.contains(INVENTORY_FIELD_SEP) {
            line.split(INVENTORY_FIELD_SEP).collect()
        } else if line.contains(INVENTORY_FIELD_SEP_ESCAPED) {
            line.split(INVENTORY_FIELD_SEP_ESCAPED).collect()
        } else {
            line.split('\t').collect()
        };
        if fields.len() != 8 && fields.len() != 11 {
            bail!(
                "failed to parse tmux inventory line {} for host `{}`: expected 8 or 11 fields, got {}",
                index + 1,
                host,
                fields.len()
            );
        }
        let session = fields[0].to_string();
        let window = fields[1].to_string();
        let pane = fields[2].to_string();
        let target = format!("{host}/{session}:{window}.{pane}");
        let window_name = fields
            .get(8)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let pane_title = fields
            .get(9)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let host_short = fields
            .get(10)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        panes.push(Pane {
            target,
            host: host.to_string(),
            session,
            window,
            pane,
            pane_id: fields[3].to_string(),
            pid: fields[4].parse().ok(),
            command: fields[5].to_string(),
            cwd: fields[6].to_string(),
            session_attached: parse_tmux_bool(fields[7]),
            window_name,
            pane_title,
            host_short,
        });
    }
    Ok(panes)
}

pub fn inventory_with_captures_command(
    capture_lines: usize,
    collect_git: bool,
    skip_git_cwds: &[String],
    socket: Option<&str>,
) -> String {
    let tmux = tmux_command(socket);
    let git_section = if collect_git {
        let skip_block = if skip_git_cwds.is_empty() {
            String::new()
        } else {
            let patterns: Vec<String> = skip_git_cwds.iter().map(|c| shell_quote(c)).collect();
            format!(
                "      case \"$_git_cwd\" in\n        {}) continue ;;\n      esac\n",
                patterns.join("|")
            )
        };
        format!(
            "\n{tmux} list-panes -a -F '#{{pane_id}}\t#{{pane_current_path}}' | while IFS='\t' read -r _git_pid _git_cwd; do\n\
      case \"$_git_cwd\" in\n\
        /|\"$HOME\"|\"\") continue ;;\n\
      esac\n\
{skip_block}\
      echo \"===REMUX-GIT-$NONCE-$_git_pid-BEGIN===\"\n\
      _toplevel=$(git -C \"$_git_cwd\" rev-parse --show-toplevel 2>/dev/null || echo \"\")\n\
      if [ -n \"$_toplevel\" ]; then\n\
        _branch=$(git -C \"$_git_cwd\" rev-parse --abbrev-ref HEAD 2>/dev/null || echo \"\")\n\
        _dirty=$(git -C \"$_git_cwd\" status --porcelain=v1 2>/dev/null | wc -l | tr -d ' ')\n\
      else\n\
        _branch=\"\"\n\
        _dirty=\"\"\n\
      fi\n\
      printf '%s\\n%s\\n%s\\n' \"$_toplevel\" \"$_branch\" \"$_dirty\"\n\
      echo \"===REMUX-GIT-$NONCE-$_git_pid-END===\"\n\
    done",
            skip_block = skip_block,
            tmux = tmux,
        )
    } else {
        String::new()
    };

    format!(
        "NONCE=$(openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | xxd -p | tr -d '\\n')\n\
echo \"===REMUX-INVENTORY-$NONCE-BEGIN===\"\n\
{inventory}\n\
echo \"===REMUX-INVENTORY-$NONCE-END===\"\n\
{tmux} list-panes -a -F '#{{pane_id}}' | while IFS= read -r pid; do\n\
    echo \"===REMUX-CAPTURE-$NONCE-$pid===\"\n\
    {tmux} capture-pane -pt \"$pid\" -S -{capture_lines} || echo \"===REMUX-ERROR-$NONCE===\"\n\
done{git_section}\n\
echo \"===REMUX-END-$NONCE===\"",
        inventory = inventory_command(socket),
        capture_lines = capture_lines,
        git_section = git_section,
        tmux = tmux,
    )
}

pub fn parse_inventory_with_captures(host_id: &str, raw: &str) -> Result<InventoryWithCaptures> {
    let mut lines = raw.lines();

    // First line must be the inventory-begin delimiter; extract nonce from it.
    let first = lines
        .next()
        .ok_or_else(|| anyhow!("combined output is empty"))?;
    let nonce = first
        .strip_prefix("===REMUX-INVENTORY-")
        .and_then(|s| s.strip_suffix("-BEGIN==="))
        .ok_or_else(|| anyhow!("expected REMUX-INVENTORY-BEGIN delimiter, got: {first:?}"))?
        .to_string();

    let inv_end = format!("===REMUX-INVENTORY-{nonce}-END===");
    let end_marker = format!("===REMUX-END-{nonce}===");
    let error_sentinel = format!("===REMUX-ERROR-{nonce}===");

    // Collect inventory lines.
    let mut inventory_lines = Vec::new();
    let mut found_inv_end = false;
    for line in lines.by_ref() {
        if line == inv_end {
            found_inv_end = true;
            break;
        }
        inventory_lines.push(line);
    }
    if !found_inv_end {
        bail!("missing inventory-end delimiter for nonce {nonce}");
    }

    let panes = parse_inventory(host_id, &inventory_lines.join("\n"))?;

    // Collect per-pane captures and git sections.
    let mut captures: HashMap<String, Option<String>> = HashMap::new();
    let mut git_map: HashMap<String, Option<crate::git::RepoSnapshot>> = HashMap::new();
    let mut current_pane_id: Option<String> = None;
    let mut current_body: Vec<&str> = Vec::new();
    let mut current_git_pid: Option<String> = None;
    let mut current_git_body: Vec<&str> = Vec::new();
    let mut found_end = false;

    let capture_prefix = format!("===REMUX-CAPTURE-{nonce}-");
    let git_begin_prefix = format!("===REMUX-GIT-{nonce}-");

    let flush_capture = |pid: String,
                         body: &[&str],
                         captures: &mut HashMap<String, Option<String>>,
                         error_sentinel: &str| {
        if body == [error_sentinel] {
            captures.insert(pid, None);
        } else {
            captures.insert(pid, Some(body.join("\n")));
        }
    };

    let flush_git =
        |pid: String,
         body: &[&str],
         git_map: &mut HashMap<String, Option<crate::git::RepoSnapshot>>| {
            // body[0..3] are toplevel/branch/dirty
            git_map.insert(pid, crate::git::parse_git_block(body));
        };

    for line in lines.by_ref() {
        if line == end_marker {
            if let Some(pid) = current_pane_id.take() {
                flush_capture(pid, &current_body, &mut captures, &error_sentinel);
                current_body.clear();
            }
            if let Some(pid) = current_git_pid.take() {
                flush_git(pid, &current_git_body, &mut git_map);
                current_git_body.clear();
            }
            found_end = true;
            break;
        }

        // Check for a git-end delimiter (reuses git_begin_prefix).
        if let Some(rest) = line.strip_prefix(&git_begin_prefix)
            && let Some(pid) = rest.strip_suffix("-END===")
            && let Some(gpid) = current_git_pid.take()
        {
            if gpid == pid {
                flush_git(gpid, &current_git_body, &mut git_map);
                current_git_body.clear();
                continue;
            }
            // Mismatched pid — treat as content (shouldn't happen).
            current_git_pid = Some(gpid);
        }

        // Check for a git-begin delimiter.
        if let Some(rest) = line.strip_prefix(&git_begin_prefix)
            && let Some(pid) = rest.strip_suffix("-BEGIN===")
        {
            // Flush any in-progress capture.
            if let Some(prev) = current_pane_id.take() {
                flush_capture(prev, &current_body, &mut captures, &error_sentinel);
                current_body.clear();
            }
            // Flush any in-progress git block.
            if let Some(gpid) = current_git_pid.take() {
                flush_git(gpid, &current_git_body, &mut git_map);
                current_git_body.clear();
            }
            current_git_pid = Some(pid.to_string());
            continue;
        }

        // Check for a capture delimiter with the correct nonce.
        if let Some(rest) = line.strip_prefix(&capture_prefix)
            && let Some(pid) = rest.strip_suffix("===")
        {
            if let Some(prev) = current_pane_id.take() {
                flush_capture(prev, &current_body, &mut captures, &error_sentinel);
                current_body.clear();
            }
            if let Some(gpid) = current_git_pid.take() {
                flush_git(gpid, &current_git_body, &mut git_map);
                current_git_body.clear();
            }
            current_pane_id = Some(pid.to_string());
            continue;
        }

        if current_git_pid.is_some() {
            current_git_body.push(line);
        } else {
            current_body.push(line);
        }
    }

    if !found_end {
        bail!("missing end terminator for nonce {nonce}");
    }

    Ok((panes, captures, git_map))
}

pub fn capture_command(
    target: &PaneTarget,
    lines: usize,
    color: bool,
    socket: Option<&str>,
) -> String {
    let color_flag = if color { "-e " } else { "" };
    format!(
        "{} capture-pane {}-pt {} -S -{}",
        tmux_command(socket),
        color_flag,
        shell_quote(&target.tmux_target()),
        lines
    )
}

pub fn new_session_command(
    session: &str,
    cwd: Option<&str>,
    window_name: Option<&str>,
    socket: Option<&str>,
) -> String {
    let mut parts = vec![
        tmux_command(socket),
        "new-session -d -s".to_string(),
        shell_quote(session),
    ];
    if let Some(cwd) = cwd {
        parts.push("-c".to_string());
        parts.push(shell_path(cwd));
    }
    if let Some(window_name) = window_name {
        parts.push("-n".to_string());
        parts.push(shell_quote(window_name));
    }
    parts.join(" ")
}

pub fn kill_session_command(session: &str, socket: Option<&str>) -> String {
    format!(
        "{} kill-session -t {}",
        tmux_command(socket),
        shell_quote(session)
    )
}

pub fn kill_pane_command(target: &PaneTarget, socket: Option<&str>) -> String {
    format!(
        "{} kill-pane -t {}",
        tmux_command(socket),
        shell_quote(&target.tmux_target())
    )
}

pub fn rename_session_command(old_name: &str, new_name: &str, socket: Option<&str>) -> String {
    format!(
        "{} rename-session -t {} {}",
        tmux_command(socket),
        shell_quote(old_name),
        shell_quote(new_name)
    )
}

pub fn split_window_command(session: &str, socket: Option<&str>) -> String {
    format!(
        "{} split-window -d -t {}",
        tmux_command(socket),
        shell_quote(session)
    )
}

pub fn send_keys_command(target: &str, keys: &str, enter: bool, socket: Option<&str>) -> String {
    let base = tmux_command(socket);
    let target = shell_quote(target);
    let mut command = format!("{base} send-keys -t {target} -l {}", shell_quote(keys));
    if enter {
        command.push_str(&format!(
            " && sleep 0.05 && {base} send-keys -t {target} Enter"
        ));
    }
    command
}

pub fn inventory_command(socket: Option<&str>) -> String {
    format!(
        "{} list-panes -a -F {INVENTORY_FORMAT}",
        tmux_command(socket)
    )
}

pub fn tmux_command(socket: Option<&str>) -> String {
    match socket.map(str::trim).filter(|socket| !socket.is_empty()) {
        Some(socket) => format!("tmux -S {}", shell_path(socket)),
        None => "tmux".to_string(),
    }
}

fn parse_tmux_bool(value: &str) -> bool {
    matches!(value, "1" | "true" | "yes" | "on")
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn shell_path(value: &str) -> String {
    if value == "~" {
        return "$HOME".to_string();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return format!("$HOME/{}", shell_quote(rest));
    }
    shell_quote(value)
}

pub fn matches_target(pane: &Pane, target: &TmuxTarget) -> bool {
    if pane.session != target.session {
        return false;
    }
    if let Some(window) = target.window
        && pane.window != window.to_string()
    {
        return false;
    }
    if let Some(pane_index) = target.pane
        && pane.pane != pane_index.to_string()
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pane_target() {
        let target = PaneTarget::parse("pi/codex:0.1").unwrap();
        assert_eq!(target.host, "pi");
        assert_eq!(target.session, "codex");
        assert_eq!(target.window, "0");
        assert_eq!(target.pane, "1");
        assert_eq!(target.to_string(), "pi/codex:0.1");
    }

    #[test]
    fn parses_tmux_inventory() {
        let output = "codex\t0\t1\t%3\t1234\tzsh\t/home/cam/work\t1\n";
        let panes = parse_inventory("pi", output).unwrap();
        assert_eq!(panes[0].target, "pi/codex:0.1");
        assert_eq!(panes[0].pid, Some(1234));
        assert_eq!(panes[0].command, "zsh");
        assert!(panes[0].session_attached);
    }

    #[test]
    fn parses_unit_separator_inventory_with_window_metadata() {
        // 11-field row using \x1f as the field delimiter, which older fixtures
        // and direct parser callers may already have materialized.
        let output = "codex\u{1f}0\u{1f}1\u{1f}%3\u{1f}1234\u{1f}zsh\u{1f}/home/cam/work\u{1f}1\u{1f}lrcp\u{1f}✳ Claude Code\u{1f}rpi\n";
        let panes = parse_inventory("pi", output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].target, "pi/codex:0.1");
        assert_eq!(panes[0].window_name.as_deref(), Some("lrcp"));
        assert_eq!(panes[0].pane_title.as_deref(), Some("✳ Claude Code"));
        assert_eq!(panes[0].host_short.as_deref(), Some("rpi"));
    }

    #[test]
    fn parses_tmux_escaped_unit_separator_inventory() {
        let output = "codex\\0370\\0371\\037%3\\0371234\\037zsh\\037/home/cam/work\\0371\\037lrcp\\037pane title\\037rpi\n";
        let panes = parse_inventory("pi", output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].target, "pi/codex:0.1");
        assert_eq!(panes[0].pid, Some(1234));
        assert_eq!(panes[0].window_name.as_deref(), Some("lrcp"));
        assert_eq!(panes[0].pane_title.as_deref(), Some("pane title"));
        assert_eq!(panes[0].host_short.as_deref(), Some("rpi"));
    }

    #[test]
    fn unit_separator_protects_against_tab_in_window_name() {
        // A user can rename a window to include a literal tab; with the old
        // tab-separated format this would have produced 12 fields and a
        // parse error. With \x1f the tab stays inside the window_name field.
        let output = "work\u{1f}0\u{1f}0\u{1f}%1\u{1f}100\u{1f}zsh\u{1f}/home/cam\u{1f}1\u{1f}foo\tbar\u{1f}\u{1f}rpi\n";
        let panes = parse_inventory("pi", output).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].window_name.as_deref(), Some("foo\tbar"));
        assert_eq!(panes[0].pane_title, None);
    }

    #[test]
    fn inventory_format_uses_unit_separator_field_delimiter() {
        // Anchor: if the format string is changed back to tab-separated,
        // the parser's split heuristic and SSH transport assumptions need
        // to be revisited together.
        assert!(INVENTORY_FORMAT.contains(INVENTORY_FIELD_SEP));
        assert!(!INVENTORY_FORMAT.contains('\t'));
    }

    #[test]
    fn capture_command_quotes_target() {
        let target = PaneTarget::parse("pi/codex:0.1").unwrap();
        assert_eq!(
            capture_command(&target, 120, false, None),
            "tmux capture-pane -pt 'codex:0.1' -S -120"
        );
    }

    #[test]
    fn capture_command_preserves_color_when_requested() {
        let target = PaneTarget::parse("pi/codex:0.1").unwrap();
        assert_eq!(
            capture_command(&target, 20, true, None),
            "tmux capture-pane -e -pt 'codex:0.1' -S -20"
        );
    }

    #[test]
    fn commands_include_custom_socket_when_configured() {
        let target = PaneTarget::parse("pi/codex:0.1").unwrap();
        assert_eq!(
            capture_command(&target, 20, false, Some("~/.work-os/tmux.sock")),
            "tmux -S $HOME/'.work-os/tmux.sock' capture-pane -pt 'codex:0.1' -S -20"
        );
        let combined = inventory_with_captures_command(2, false, &[], Some("/tmp/tmux.sock"));
        assert!(combined.contains("tmux -S '/tmp/tmux.sock' list-panes -a -F '#S"));
        assert!(combined.contains("tmux -S '/tmp/tmux.sock' capture-pane -pt \"$pid\" -S -2"));
    }

    #[test]
    fn lifecycle_commands_quote_targets() {
        assert_eq!(
            new_session_command("work", Some("~/repo"), Some("main"), None),
            "tmux new-session -d -s 'work' -c $HOME/'repo' -n 'main'"
        );
        assert_eq!(
            kill_session_command("work", None),
            "tmux kill-session -t 'work'"
        );
        assert_eq!(
            rename_session_command("work", "work-next", None),
            "tmux rename-session -t 'work' 'work-next'"
        );
        assert_eq!(
            split_window_command("work", None),
            "tmux split-window -d -t 'work'"
        );
        assert_eq!(
            send_keys_command("work:0.1", "cargo test", true, None),
            "tmux send-keys -t 'work:0.1' -l 'cargo test' && sleep 0.05 && tmux send-keys -t 'work:0.1' Enter"
        );
        assert_eq!(
            send_keys_command("work", "q", false, Some("/tmp/tmux.sock")),
            "tmux -S '/tmp/tmux.sock' send-keys -t 'work' -l 'q'"
        );
    }

    #[test]
    fn shell_path_expands_remote_home() {
        assert_eq!(shell_path("~/work/repo"), "$HOME/'work/repo'");
        assert_eq!(shell_path("/tmp/repo"), "'/tmp/repo'");
    }

    // --- parse_inventory_with_captures tests ---

    fn make_combined(nonce: &str, inv: &str, captures: &[(&str, &str)]) -> String {
        let mut out = format!(
            "===REMUX-INVENTORY-{nonce}-BEGIN===\n{inv}\n===REMUX-INVENTORY-{nonce}-END===\n"
        );
        for (pid, body) in captures {
            out.push_str(&format!("===REMUX-CAPTURE-{nonce}-{pid}===\n{body}\n"));
        }
        out.push_str(&format!("===REMUX-END-{nonce}===\n"));
        out
    }

    #[test]
    fn parse_combined_happy_path() {
        let inv = "work\t0\t0\t%1\t100\tzsh\t/home/cam\t1\nwork\t0\t1\t%2\t101\tbash\t/tmp\t0";
        let raw = make_combined("abc123", inv, &[("%1", "line1\nline2"), ("%2", "")]);
        let (panes, captures, _git) = parse_inventory_with_captures("host", &raw).unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%1");
        assert_eq!(panes[1].pane_id, "%2");
        assert_eq!(captures["%1"], Some("line1\nline2".to_string()));
        assert_eq!(captures["%2"], Some("".to_string()));
    }

    #[test]
    fn parse_combined_missing_terminator_errors() {
        let inv = "work\t0\t0\t%1\t100\tzsh\t/home/cam\t1";
        let nonce = "deadbeef";
        let raw = format!(
            "===REMUX-INVENTORY-{nonce}-BEGIN===\n{inv}\n===REMUX-INVENTORY-{nonce}-END===\n===REMUX-CAPTURE-{nonce}-%1===\nsome output\n"
        );
        let err = parse_inventory_with_captures("host", &raw).unwrap_err();
        assert!(err.to_string().contains("missing end terminator"));
    }

    #[test]
    fn parse_combined_wrong_nonce_in_body_is_content() {
        let inv = "work\t0\t0\t%1\t100\tzsh\t/home/cam\t1";
        // A line that looks like a delimiter but has a different nonce — should be capture content.
        let body_line = "===REMUX-CAPTURE-wrongnonce-%2===";
        let raw = make_combined("abc123", inv, &[("%1", body_line)]);
        let (panes, captures, _git) = parse_inventory_with_captures("host", &raw).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(captures["%1"], Some(body_line.to_string()));
    }

    #[test]
    fn parse_combined_empty_capture() {
        let inv = "work\t0\t0\t%1\t100\tzsh\t/home/cam\t1";
        let raw = make_combined("abc123", inv, &[("%1", "")]);
        let (_, captures, _git) = parse_inventory_with_captures("host", &raw).unwrap();
        assert_eq!(captures["%1"], Some("".to_string()));
    }

    #[test]
    fn parse_combined_error_sentinel_gives_none() {
        let inv = "work\t0\t0\t%1\t100\tzsh\t/home/cam\t1";
        let nonce = "abc123";
        // Simulate what the shell emits when capture-pane fails: the error sentinel as the body.
        let raw = format!(
            "===REMUX-INVENTORY-{nonce}-BEGIN===\n{inv}\n===REMUX-INVENTORY-{nonce}-END===\n===REMUX-CAPTURE-{nonce}-%1===\n===REMUX-ERROR-{nonce}===\n===REMUX-END-{nonce}===\n"
        );
        let (_, captures, _git) = parse_inventory_with_captures("host", &raw).unwrap();
        assert_eq!(captures["%1"], None);
    }

    #[test]
    fn parse_combined_git_section() {
        let nonce = "abc123";
        let inv = "work\t0\t0\t%1\t100\tzsh\t/home/cam/repo\t1";
        let raw = format!(
            "===REMUX-INVENTORY-{nonce}-BEGIN===\n{inv}\n===REMUX-INVENTORY-{nonce}-END===\n\
===REMUX-CAPTURE-{nonce}-%1===\nhello\n\
===REMUX-GIT-{nonce}-%1-BEGIN===\n/home/cam/repo\nmain\n2\n===REMUX-GIT-{nonce}-%1-END===\n\
===REMUX-END-{nonce}===\n"
        );
        let (_, _, git) = parse_inventory_with_captures("host", &raw).unwrap();
        let snap = git["%1"].as_ref().unwrap();
        assert_eq!(snap.path, "/home/cam/repo");
        assert_eq!(snap.branch, Some("main".to_string()));
        assert_eq!(snap.dirty_count, Some(2));
    }

    #[test]
    fn parse_combined_git_not_in_repo() {
        let nonce = "abc123";
        let inv = "work\t0\t0\t%1\t100\tzsh\t/tmp\t1";
        let raw = format!(
            "===REMUX-INVENTORY-{nonce}-BEGIN===\n{inv}\n===REMUX-INVENTORY-{nonce}-END===\n\
===REMUX-CAPTURE-{nonce}-%1===\nhello\n\
===REMUX-GIT-{nonce}-%1-BEGIN===\n\n\n\n===REMUX-GIT-{nonce}-%1-END===\n\
===REMUX-END-{nonce}===\n"
        );
        let (_, _, git) = parse_inventory_with_captures("host", &raw).unwrap();
        assert!(git["%1"].is_none());
    }

    #[test]
    fn collect_git_flag_controls_git_block() {
        let off = inventory_with_captures_command(2, false, &[], None);
        assert!(!off.contains("REMUX-GIT"));
        let on = inventory_with_captures_command(2, true, &[], None);
        assert!(on.contains("REMUX-GIT-$NONCE"));
        assert!(on.contains("git -C"));
    }
}
