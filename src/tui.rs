use crate::attach;
use crate::config::{Config, TuiSortDirection, TuiSortField};
use crate::dir_picker;
use crate::launch_template::{
    LaunchTemplatePreset, launch_session_name, launch_template_label, launch_template_presets,
};
use crate::lifecycle;
use crate::snapshot::{
    self, HostSnapshot, MatchStatus, PaneDetail, SessionSnapshot, SessionState, SnapshotError,
    SnapshotStatus,
};
use crate::tmux::PaneTarget;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, Event, KeyCode, KeyEvent,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
use std::io::{self, Stdout};
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

mod line_editor;
use line_editor::LineEditor;

pub fn run(config: &Config, host: Option<String>, filter: Option<String>) -> Result<()> {
    if let Some(host_id) = &host {
        config.host(host_id)?;
    }

    let saved_terminal_mode = SavedTerminalMode::capture()?;
    let mut terminal = enter_terminal()?;
    let result = run_app(
        config.clone(),
        host,
        filter.unwrap_or_default(),
        &mut terminal,
    );
    leave_terminal(&mut terminal)?;
    saved_terminal_mode.restore()?;
    result
}

fn run_app(
    config: Config,
    host: Option<String>,
    filter: String,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(filter, config.tui.sort.field, config.tui.sort.direction);
    spawn_refresh(&config, host.clone(), tx.clone(), &mut app)?;

    loop {
        while let Ok(message) = rx.try_recv() {
            app.apply_refresh(message);
        }

        terminal.draw(|frame| draw(frame, &app))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && handle_key(key, &config, host.clone(), tx.clone(), &mut app, terminal)?
        {
            break;
        }

        // Auto-refresh: if enough time has elapsed since the last refresh completed
        // (or started, if it never completed), kick off a background refresh.
        if should_auto_refresh(&config, &app) {
            spawn_refresh(&config, host.clone(), tx.clone(), &mut app)?;
        }
    }

    Ok(())
}

fn enter_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        EnterAlternateScreen
    )
    .context("failed to reset terminal modes and enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to initialize terminal")
}

fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")
}

fn suspend_terminal<T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    leave_terminal(terminal)?;
    let saved_terminal_mode = SavedTerminalMode::capture()?;
    let result = action();
    // An abruptly terminated ssh/tmux client is not guaranteed to restore the
    // local TTY. In particular, a proxy can leave stdin in its raw mode while
    // remux successfully returns to its alternate screen, making the TUI look
    // alive while it no longer receives usable key events. Restore the mode
    // captured before the interactive action before crossterm establishes a
    // new raw-mode baseline.
    saved_terminal_mode.restore()?;
    *terminal = enter_terminal()?;
    result
}

struct SavedTerminalMode {
    #[cfg(unix)]
    mode: libc::termios,
}

impl SavedTerminalMode {
    fn capture() -> Result<Self> {
        #[cfg(unix)]
        {
            let tty =
                TerminalFile::open().context("failed to open terminal before starting TUI")?;
            Self::capture_fd(tty.fd())
                .context("failed to capture terminal mode before starting TUI")
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    fn restore(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let tty = TerminalFile::open()
                .context("failed to open terminal after interactive command")?;
            self.restore_fd(tty.fd())
                .context("failed to restore terminal mode after interactive command")?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn capture_fd(fd: RawFd) -> io::Result<Self> {
        terminal_mode(fd).map(|mode| Self { mode })
    }

    #[cfg(unix)]
    fn restore_fd(&self, fd: RawFd) -> io::Result<()> {
        set_terminal_mode(fd, &self.mode)
    }
}

#[cfg(unix)]
enum TerminalFile {
    Stdin,
    Tty(File),
}

#[cfg(unix)]
impl TerminalFile {
    fn open() -> io::Result<Self> {
        // SAFETY: isatty only inspects the process-owned stdin descriptor.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            Ok(Self::Stdin)
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .map(Self::Tty)
        }
    }

    fn fd(&self) -> RawFd {
        match self {
            Self::Stdin => libc::STDIN_FILENO,
            Self::Tty(file) => file.as_raw_fd(),
        }
    }
}

#[cfg(unix)]
fn terminal_mode(fd: libc::c_int) -> io::Result<libc::termios> {
    let mut mode = MaybeUninit::uninit();
    // SAFETY: `mode` points to writable storage for a termios value and `fd`
    // is only passed to libc for the duration of this call.
    if unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: tcgetattr returned success and initialized the termios value.
    Ok(unsafe { mode.assume_init() })
}

#[cfg(unix)]
fn set_terminal_mode(fd: libc::c_int, mode: &libc::termios) -> io::Result<()> {
    // SAFETY: `mode` is a valid termios value captured by tcgetattr and remains
    // alive for the duration of the call.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, mode) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct App {
    snapshots: Vec<HostSnapshot>,
    selected: usize,
    selected_identity: Option<String>,
    table_offset: usize,
    filter: LineEditor,
    editing_filter: bool,
    help: bool,
    status: String,
    host_progress: Vec<HostProgress>,
    refresh_started_at: Option<Instant>,
    refresh_completed_at: Option<Instant>,
    last_refresh_duration: Option<Duration>,
    captured_for: Option<String>,
    captured_output: Option<String>,
    inspected_for: Option<String>,
    inspected_detail: Option<PaneDetail>,
    kill_prompt: Option<KillPrompt>,
    input_prompt: Option<InputPrompt>,
    template_prompt: Option<TemplatePrompt>,
    sort_field: TuiSortField,
    sort_direction: TuiSortDirection,
    show_context: bool,
    inspect_mode: bool,
    inspect_pending: Option<String>,
    pending_selection: Option<SelectionPreference>,
    pending_key: Option<PendingKey>,
    refresh_id: RefreshId,
}

#[derive(Clone)]
struct SelectionPreference {
    identity: String,
    host: String,
    session: String,
}

impl SelectionPreference {
    fn new_session(host: &str, session: &str) -> Self {
        Self {
            identity: format!("target:{host}/{session}:0.0"),
            host: host.to_string(),
            session: session.to_string(),
        }
    }
}

struct KillPrompt {
    target: String,
}

struct InputPrompt {
    kind: InputPromptKind,
    value: LineEditor,
}

struct TemplatePrompt {
    step: TemplatePromptStep,
}

enum TemplatePromptStep {
    Host {
        value: LineEditor,
    },
    Preset {
        host: String,
        presets: Vec<LaunchTemplatePreset>,
        selected: usize,
    },
    Name {
        host: String,
        preset: LaunchTemplatePreset,
        value: LineEditor,
    },
}

#[derive(Clone)]
enum InputPromptKind {
    RenameSession {
        host: String,
        current: String,
    },
    NewSession,
    NewSessionCwd {
        host: String,
        session: String,
    },
    LaunchTemplateCwd {
        host: String,
        preset: LaunchTemplatePreset,
        suffix: String,
    },
    NewPane,
    SendKeys {
        target: String,
    },
}

#[derive(Clone)]
struct HostProgress {
    id: String,
    state: HostProgressState,
    started_at: Option<Instant>,
    finished_at: Option<Instant>,
    message: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostProgressState {
    Queued,
    Polling,
    Ok,
    Unreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKey {
    G,
}

#[derive(Clone, Copy)]
enum VisiblePosition {
    Top,
    Middle,
    Bottom,
}

impl App {
    fn new(filter: String, sort_field: TuiSortField, sort_direction: TuiSortDirection) -> Self {
        Self {
            snapshots: Vec::new(),
            selected: 0,
            selected_identity: None,
            table_offset: 0,
            filter: LineEditor::new(filter),
            editing_filter: false,
            help: false,
            status: "polling".to_string(),
            host_progress: Vec::new(),
            refresh_started_at: None,
            refresh_completed_at: None,
            last_refresh_duration: None,
            captured_for: None,
            captured_output: None,
            inspected_for: None,
            inspected_detail: None,
            kill_prompt: None,
            input_prompt: None,
            template_prompt: None,
            sort_field,
            sort_direction,
            show_context: true,
            inspect_mode: false,
            inspect_pending: None,
            pending_selection: None,
            pending_key: None,
            refresh_id: 0,
        }
    }

    fn rows(&self) -> Vec<&SessionSnapshot> {
        let filter = self.filter.as_str().trim().to_lowercase();
        let mut rows: Vec<&SessionSnapshot> = self
            .snapshots
            .iter()
            .flat_map(|snapshot| snapshot.sessions.iter())
            .filter(|session| {
                filter.is_empty()
                    || row_search_text(session)
                        .to_lowercase()
                        .contains(filter.as_str())
            })
            .collect();
        rows.sort_by(|left, right| self.compare_tree_rows(left, right));
        rows
    }

    fn selected_row(&self) -> Option<&SessionSnapshot> {
        let rows = self.rows();
        rows.get(self.selected).copied()
    }

    fn selected_row_identity(&self) -> Option<String> {
        self.selected_row().map(row_identity)
    }

    fn restore_selection(&mut self, preferred_identity: Option<String>) -> bool {
        let preferred_identity = preferred_identity.or_else(|| self.selected_identity.clone());
        let (selected, selected_identity) = {
            let rows = self.rows();
            if rows.is_empty() {
                self.selected = 0;
                self.selected_identity = None;
                self.table_offset = 0;
                return false;
            }
            let selected = if let Some(identity) = preferred_identity {
                rows.iter()
                    .position(|row| row_identity(row) == identity)
                    .unwrap_or_else(|| self.selected.min(rows.len() - 1))
            } else {
                self.selected.min(rows.len() - 1)
            };
            (selected, rows.get(selected).map(|row| row_identity(row)))
        };
        if selected_identity.is_none() {
            self.selected = 0;
            self.selected_identity = None;
            self.table_offset = 0;
            return false;
        }
        self.selected = selected;
        self.selected_identity = selected_identity;
        true
    }

    fn restore_selection_after_refresh(&mut self, fallback_identity: Option<String>) {
        if self.restore_pending_selection() {
            return;
        }
        self.restore_selection(fallback_identity);
    }

    fn restore_pending_selection(&mut self) -> bool {
        let Some(preference) = self.pending_selection.clone() else {
            return false;
        };
        let selected = {
            let rows = self.rows();
            let exact = rows
                .iter()
                .position(|row| row_identity(row) == preference.identity);
            let fallback = exact.or_else(|| {
                rows.iter().position(|row| {
                    row.raw_target.is_some()
                        && row.host == preference.host
                        && row.tmux.session == preference.session
                })
            });
            fallback.map(|index| (index, row_identity(rows[index])))
        };
        let Some((selected, selected_identity)) = selected else {
            return false;
        };
        self.selected = selected;
        self.selected_identity = Some(selected_identity);
        self.pending_selection = None;
        true
    }

    fn select_pane_index(&mut self, index: usize) {
        let selected = {
            let rows = self.rows();
            if rows.is_empty() {
                self.selected = 0;
                self.selected_identity = None;
                self.table_offset = 0;
                return;
            }
            let selected = index.min(rows.len() - 1);
            let selected_identity = row_identity(rows[selected]);
            (selected, selected_identity)
        };
        self.selected = selected.0;
        self.selected_identity = Some(selected.1);
    }

    fn select_first_pane(&mut self) {
        self.select_pane_index(0);
    }

    fn select_last_pane(&mut self) {
        let Some(last_index) = self.rows().len().checked_sub(1) else {
            self.select_pane_index(0);
            return;
        };
        self.select_pane_index(last_index);
    }

    fn move_selection_by(&mut self, delta: isize) {
        let rows_len = self.rows().len();
        if rows_len == 0 {
            self.select_pane_index(0);
            return;
        }
        let max_index = rows_len - 1;
        let selected = self.selected.saturating_add_signed(delta).min(max_index);
        self.select_pane_index(selected);
    }

    fn move_page(&mut self, delta_pages: isize, visible_rows: usize) {
        self.move_display_rows_by(delta_pages.saturating_mul(visible_rows.max(1) as isize));
    }

    fn move_half_page(&mut self, delta: isize, visible_rows: usize) {
        let half_page = (visible_rows.max(1) / 2).max(1);
        self.move_display_rows_by(delta.saturating_mul(half_page as isize));
    }

    fn select_visible_position(&mut self, position: VisiblePosition, visible_rows: usize) {
        let selected = {
            let rows = self.rows();
            let display_rows = live_table_rows(&rows);
            if let Some(last_display_index) = display_rows.len().checked_sub(1) {
                let start = self.table_offset.min(last_display_index);
                let end = start
                    .saturating_add(visible_rows.max(1).saturating_sub(1))
                    .min(last_display_index);
                let target = match position {
                    VisiblePosition::Top => start,
                    VisiblePosition::Middle => start + (end - start) / 2,
                    VisiblePosition::Bottom => end,
                };
                pane_index_for_visible_position(&display_rows, start, end, target, position)
                    .or_else(|| nearest_pane_index_for_display_row(&display_rows, target))
            } else {
                None
            }
        };
        if let Some(selected) = selected {
            self.select_pane_index(selected);
        }
    }

    fn ensure_selection_visible(&mut self, visible_rows: usize) {
        let rows = self.rows();
        let display_rows = live_table_rows(&rows);
        let Some(selected_display_index) = display_index_for_pane(&display_rows, self.selected)
        else {
            self.table_offset = 0;
            return;
        };
        let visible_rows = visible_rows.max(1);
        let max_offset = display_rows.len().saturating_sub(visible_rows);
        self.table_offset = self.table_offset.min(max_offset);
        if selected_display_index < self.table_offset {
            self.table_offset = selected_display_index;
        } else if selected_display_index >= self.table_offset + visible_rows {
            self.table_offset = selected_display_index + 1 - visible_rows;
        }
        self.table_offset = self.table_offset.min(max_offset);
    }

    fn move_display_rows_by(&mut self, delta: isize) {
        let selected = {
            let rows = self.rows();
            let display_rows = live_table_rows(&rows);
            let Some(current_display_index) = display_index_for_pane(&display_rows, self.selected)
            else {
                return;
            };
            let Some(last_display_index) = display_rows.len().checked_sub(1) else {
                return;
            };
            let target_display_index = current_display_index
                .saturating_add_signed(delta)
                .min(last_display_index);
            nearest_pane_index_for_directional_display_row(
                &display_rows,
                target_display_index,
                delta,
            )
        };
        if let Some(selected) = selected {
            self.select_pane_index(selected);
        }
    }

    fn compare_tree_rows(&self, left: &SessionSnapshot, right: &SessionSnapshot) -> Ordering {
        self.host_rank(&left.host)
            .cmp(&self.host_rank(&right.host))
            .then_with(|| left.tmux_socket.cmp(&right.tmux_socket))
            .then_with(|| left.container.cmp(&right.container))
            .then_with(|| left.tmux.session.cmp(&right.tmux.session))
            .then_with(|| {
                compare_tmux_index(left.tmux.window.as_deref(), right.tmux.window.as_deref())
            })
            .then_with(|| compare_sort_field(left, right, self.sort_field, self.sort_direction))
            .then_with(|| stable_row_cmp(left, right))
    }

    fn host_rank(&self, host: &str) -> usize {
        self.snapshots
            .iter()
            .position(|snapshot| snapshot.host == host)
            .unwrap_or(usize::MAX)
    }

    fn begin_refresh(&mut self, host_ids: Vec<String>) -> RefreshId {
        self.refresh_id = self.refresh_id.wrapping_add(1);
        self.refresh_started_at = Some(Instant::now());
        self.refresh_completed_at = None;
        self.host_progress = host_ids
            .into_iter()
            .map(|id| HostProgress {
                id,
                state: HostProgressState::Queued,
                started_at: None,
                finished_at: None,
                message: None,
            })
            .collect();
        self.status = self.progress_summary();
        self.refresh_id
    }

    fn apply_refresh(&mut self, message: RefreshMessage) {
        match message {
            RefreshMessage::HostStarted { refresh_id, host } if refresh_id == self.refresh_id => {
                if let Some(progress) = self.host_progress.iter_mut().find(|item| item.id == host) {
                    progress.state = HostProgressState::Polling;
                    progress.started_at = Some(Instant::now());
                    progress.message = None;
                }
                self.status = self.progress_summary();
            }
            RefreshMessage::HostStarted { .. } => {}
            RefreshMessage::HostFinished {
                refresh_id,
                snapshot,
            } if refresh_id == self.refresh_id => {
                let selected_identity = self.selected_row_identity();
                self.update_host_finished(&snapshot);
                self.upsert_snapshot(snapshot);
                self.restore_selection_after_refresh(selected_identity);
                self.status = self.progress_summary();
            }
            RefreshMessage::HostFinished { .. } => {}
            RefreshMessage::Complete { refresh_id } if refresh_id == self.refresh_id => {
                if !self.is_polling() && self.refresh_completed_at.is_some() {
                    return;
                }
                // Force any still-pending hosts to Unreachable. If we get here
                // with Queued/Polling entries, poll_hosts detached wedged
                // workers past the deadline. Marking them terminal keeps
                // is_polling() honest so the UI can refresh again.
                let timeout_message = self
                    .refresh_started_at
                    .map(|started| {
                        format!("poll timed out after {}", format_elapsed(started.elapsed()))
                    })
                    .unwrap_or_else(|| "poll timed out".to_string());
                for progress in &mut self.host_progress {
                    if progress.state == HostProgressState::Queued
                        || progress.state == HostProgressState::Polling
                    {
                        progress.state = HostProgressState::Unreachable;
                        progress.finished_at = Some(Instant::now());
                        progress.message = Some(timeout_message.clone());
                    }
                }
                self.last_refresh_duration = self.refresh_started_at.map(|t| t.elapsed());
                self.refresh_completed_at = Some(Instant::now());
                self.restore_pending_selection();
                self.status = format!("scan complete: {}", self.progress_summary());
            }
            RefreshMessage::Complete { .. } => {}
            RefreshMessage::InspectResult { display_id, result } => {
                if self.inspect_pending.as_deref() == Some(display_id.as_str()) {
                    self.inspect_pending = None;
                }
                match result {
                    Ok(detail) => {
                        self.status = format!("inspected {display_id}");
                        self.inspected_for = Some(display_id);
                        self.inspected_detail = Some(detail);
                    }
                    Err(err) => {
                        self.status = format!("inspect error: {err:#}");
                    }
                }
            }
        }
    }

    fn update_host_finished(&mut self, snapshot: &HostSnapshot) {
        if let Some(progress) = self
            .host_progress
            .iter_mut()
            .find(|item| item.id == snapshot.host)
        {
            progress.state = match snapshot.status {
                snapshot::SnapshotStatus::Ok => HostProgressState::Ok,
                snapshot::SnapshotStatus::Unreachable => HostProgressState::Unreachable,
            };
            progress.finished_at = Some(Instant::now());
            progress.message = snapshot.errors.first().map(|error| error.message.clone());
        }
    }

    fn upsert_snapshot(&mut self, snapshot: HostSnapshot) {
        if let Some(existing) = self
            .snapshots
            .iter_mut()
            .find(|existing| existing.host == snapshot.host)
        {
            *existing = snapshot;
        } else {
            self.snapshots.push(snapshot);
        }

        let order: Vec<String> = self
            .host_progress
            .iter()
            .map(|progress| progress.id.clone())
            .collect();
        self.snapshots.sort_by_key(|snapshot| {
            order
                .iter()
                .position(|host| host == &snapshot.host)
                .unwrap_or(usize::MAX)
        });
    }

    fn progress_summary(&self) -> String {
        if self.host_progress.is_empty() {
            return "no hosts queued".to_string();
        }

        let total = self.host_progress.len();
        let finished = self
            .host_progress
            .iter()
            .filter(|host| host.state.is_finished())
            .count();
        let queued = self
            .host_progress
            .iter()
            .filter(|host| host.state == HostProgressState::Queued)
            .count();
        let polling: Vec<String> = self
            .host_progress
            .iter()
            .filter(|host| host.state == HostProgressState::Polling)
            .map(|host| {
                let elapsed = host
                    .started_at
                    .map(|started| format_elapsed(started.elapsed()))
                    .unwrap_or_else(|| "0s".to_string());
                format!("{} {elapsed}", host.id)
            })
            .collect();

        let mut parts = vec![format!("{finished}/{total} hosts")];
        if !polling.is_empty() {
            parts.push(format!("polling {}", polling.join(", ")));
        }
        if queued > 0 {
            parts.push(format!("{queued} queued"));
        }
        if let Some(s) = self.refresh_status_str() {
            parts.push(s);
        }
        parts.join(" | ")
    }

    fn is_polling(&self) -> bool {
        self.host_progress
            .iter()
            .any(|p| p.state == HostProgressState::Polling || p.state == HostProgressState::Queued)
    }

    fn refresh_status_str(&self) -> Option<String> {
        if self.is_polling() {
            Some(match self.refresh_started_at {
                Some(t) => format!("polling {}", format_elapsed(t.elapsed())),
                None => "polling".to_string(),
            })
        } else if let (Some(duration), Some(completed_at)) =
            (self.last_refresh_duration, self.refresh_completed_at)
        {
            Some(format!(
                "polled in {} · {} ago",
                format_elapsed(duration),
                format_elapsed(completed_at.elapsed())
            ))
        } else {
            None
        }
    }

    fn cycle_sort_field(&mut self) {
        let selected_identity = self.selected_row_identity();
        self.sort_field = match self.sort_field {
            TuiSortField::Attention => TuiSortField::LastOutput,
            TuiSortField::LastOutput => TuiSortField::State,
            TuiSortField::State => TuiSortField::Id,
            TuiSortField::Id => TuiSortField::Attention,
        };
        self.restore_selection(selected_identity);
    }

    fn toggle_sort_direction(&mut self) {
        let selected_identity = self.selected_row_identity();
        self.sort_direction = match self.sort_direction {
            TuiSortDirection::Asc => TuiSortDirection::Desc,
            TuiSortDirection::Desc => TuiSortDirection::Asc,
        };
        self.restore_selection(selected_identity);
    }
}

impl HostProgressState {
    fn is_finished(self) -> bool {
        matches!(self, HostProgressState::Ok | HostProgressState::Unreachable)
    }
}

#[allow(clippy::large_enum_variant)]
enum RefreshMessage {
    HostStarted {
        refresh_id: RefreshId,
        host: String,
    },
    HostFinished {
        refresh_id: RefreshId,
        snapshot: HostSnapshot,
    },
    Complete {
        refresh_id: RefreshId,
    },
    InspectResult {
        display_id: String,
        result: anyhow::Result<PaneDetail>,
    },
}

type RefreshId = u64;

fn spawn_refresh(
    config: &Config,
    host: Option<String>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
) -> Result<()> {
    if app.is_polling() {
        return Ok(());
    }
    let host_ids = host_ids_for_refresh(config, host.as_deref())?;
    let refresh_id = app.begin_refresh(host_ids.clone());
    let config = config.clone();
    let watchdog_tx = tx.clone();
    let watchdog_timeout = refresh_deadline(&config, &host_ids);
    thread::spawn(move || {
        thread::sleep(watchdog_timeout);
        let _ = watchdog_tx.send(RefreshMessage::Complete { refresh_id });
    });
    thread::spawn(move || {
        poll_hosts(&config, host_ids, refresh_id, tx);
    });
    Ok(())
}

fn should_auto_refresh(config: &Config, app: &App) -> bool {
    let interval = config.poll.auto_refresh_interval;
    if interval.is_zero() || app.is_polling() {
        return false;
    }
    if app.input_prompt.is_some() || app.kill_prompt.is_some() || app.editing_filter {
        return false;
    }
    match app.refresh_completed_at {
        None => true,
        Some(completed_at) => completed_at.elapsed() >= interval,
    }
}

fn spawn_inspect(config: &Config, tx: mpsc::Sender<RefreshMessage>, app: &mut App) {
    let Some(row) = app.selected_row() else {
        app.status = "no selected row".to_string();
        return;
    };
    // If already pending for this row, ignore (one in-flight at a time).
    if app.inspect_pending.as_deref() == Some(row.display_id.as_str()) {
        return;
    }
    let display_id = row.display_id.clone();
    let snapshot = row.clone();
    app.inspect_pending = Some(display_id.clone());
    app.status = format!("inspecting {display_id}…");
    let config = config.clone();
    thread::spawn(move || {
        let result = snapshot::refresh_capture_for(&config, &snapshot);
        let _ = tx.send(RefreshMessage::InspectResult { display_id, result });
    });
}

fn host_ids_for_refresh(config: &Config, host: Option<&str>) -> Result<Vec<String>> {
    Ok(match host {
        Some(host) => vec![config.host(host)?.id.clone()],
        None => config.hosts.iter().map(|host| host.id.clone()).collect(),
    })
}

fn poll_hosts(
    config: &Config,
    host_ids: Vec<String>,
    refresh_id: RefreshId,
    tx: mpsc::Sender<RefreshMessage>,
) {
    if host_ids.is_empty() {
        let _ = tx.send(RefreshMessage::Complete { refresh_id });
        return;
    }

    let worker_count = config.poll.max_concurrency.min(host_ids.len()).max(1);
    let deadline = Instant::now() + refresh_deadline(config, &host_ids);
    let queue = Arc::new(Mutex::new(VecDeque::from(host_ids)));
    let mut handles = Vec::new();

    for _ in 0..worker_count {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        let config = config.clone();
        handles.push(thread::spawn(move || {
            loop {
                let host_id = {
                    let mut queue = queue.lock().expect("poll queue poisoned");
                    queue.pop_front()
                };
                let Some(host_id) = host_id else {
                    break;
                };
                if tx
                    .send(RefreshMessage::HostStarted {
                        refresh_id,
                        host: host_id.clone(),
                    })
                    .is_err()
                {
                    break;
                }
                let snapshot = snapshot::snapshot_host(&config, &host_id)
                    .unwrap_or_else(|err| synthetic_error_snapshot(host_id, err));
                if tx
                    .send(RefreshMessage::HostFinished {
                        refresh_id,
                        snapshot,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    // Give workers up to the full snapshot budget to finish cleanly. Beyond
    // that, detach remaining workers and send Complete so the UI can resume
    // refreshes.
    for handle in handles {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break; // deadline passed; detach remaining workers
        }
        let start = Instant::now();
        while start.elapsed() < remaining && !handle.is_finished() {
            thread::sleep(Duration::from_millis(25));
        }
        if handle.is_finished() {
            let _ = handle.join();
        }
        // else: detach — thread continues in background but we move on
    }
    let _ = tx.send(RefreshMessage::Complete { refresh_id });
}

fn refresh_deadline(config: &Config, host_ids: &[String]) -> Duration {
    let command_units = host_ids
        .iter()
        .map(|host_id| snapshot_command_budget_units(config, host_id))
        .sum::<usize>()
        .max(1);
    config
        .poll
        .command_timeout
        .saturating_mul(command_units.min(u32::MAX as usize) as u32)
        + Duration::from_secs(2)
}

fn snapshot_command_budget_units(config: &Config, host_id: &str) -> usize {
    let configured_repos = config
        .watches_for_host(host_id)
        .into_iter()
        .filter_map(|watch| watch.watch.repo)
        .collect::<HashSet<_>>()
        .len();

    // Full host snapshot budget:
    // - one inventory/capture command
    // - two git commands for each unique configured watch repo
    1 + configured_repos.saturating_mul(2)
}

fn synthetic_error_snapshot(host: String, err: anyhow::Error) -> HostSnapshot {
    HostSnapshot {
        host,
        tmux_socket: None,
        status: snapshot::SnapshotStatus::Unreachable,
        collected_at: Utc::now(),
        sessions: Vec::new(),
        errors: vec![SnapshotError {
            kind: "poll".to_string(),
            message: format!("{err:#}"),
        }],
    }
}

fn handle_key(
    key: KeyEvent,
    config: &Config,
    host: Option<String>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }

    if app.kill_prompt.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let prompt = app.kill_prompt.take().expect("prompt exists");
                match lifecycle::kill(config, &prompt.target, true, false) {
                    Ok(()) => {
                        app.status = format!("killed {}", prompt.target);
                        spawn_refresh(config, host, tx, app)?;
                    }
                    Err(err) => app.status = format!("{err:#}"),
                }
            }
            _ => {
                app.kill_prompt = None;
                app.status = "kill cancelled".to_string();
            }
        }
        return Ok(false);
    }

    if app.input_prompt.is_some() {
        handle_input_prompt_key(key, config, host, tx, app, terminal)?;
        return Ok(false);
    }

    if app.template_prompt.is_some() {
        handle_template_prompt_key(key, config, host, tx, app, terminal)?;
        return Ok(false);
    }

    if app.editing_filter {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => app.editing_filter = false,
            _ => {
                let selected_identity = app.selected_row_identity();
                if app.filter.apply_key(key) {
                    app.restore_selection(selected_identity);
                    app.ensure_selection_visible(visible_table_rows(terminal)?);
                }
            }
        }
        return Ok(false);
    }

    let visible_rows = visible_table_rows(terminal)?;
    if handle_navigation_key(app, key, visible_rows) {
        return Ok(false);
    }

    match key.code {
        KeyCode::Esc => {
            if app.inspect_mode {
                app.inspect_mode = false;
                app.status = "browse".to_string();
                Ok(false)
            } else if app.help {
                app.help = false;
                Ok(false)
            } else {
                Ok(true)
            }
        }
        KeyCode::Char('q') => Ok(true),
        KeyCode::Char('/') => {
            app.editing_filter = true;
            Ok(false)
        }
        KeyCode::Char('?') => {
            app.help = !app.help;
            Ok(false)
        }
        KeyCode::Char('s') => {
            app.cycle_sort_field();
            app.ensure_selection_visible(visible_rows);
            app.status = format!("sort: {}", sort_label(app.sort_field, app.sort_direction));
            Ok(false)
        }
        KeyCode::Char('S') => {
            app.toggle_sort_direction();
            app.ensure_selection_visible(visible_rows);
            app.status = format!("sort: {}", sort_label(app.sort_field, app.sort_direction));
            Ok(false)
        }
        KeyCode::Char('r') => {
            spawn_refresh(config, host, tx, app)?;
            Ok(false)
        }
        KeyCode::Char('x') => {
            begin_kill_prompt(config, app)?;
            Ok(false)
        }
        KeyCode::Char('e') => {
            begin_rename_prompt(app);
            Ok(false)
        }
        KeyCode::Char('n') => {
            app.input_prompt = Some(InputPrompt {
                kind: InputPromptKind::NewSession,
                value: LineEditor::default(),
            });
            app.status = "new session: enter <host>/<session>".to_string();
            Ok(false)
        }
        KeyCode::Char('p') => {
            app.input_prompt = Some(InputPrompt {
                kind: InputPromptKind::NewPane,
                value: LineEditor::default(),
            });
            app.status = "new pane: enter <host>/<session>".to_string();
            Ok(false)
        }
        KeyCode::Char('t') => {
            begin_template_prompt(host.as_deref(), app);
            Ok(false)
        }
        KeyCode::Char('z') => {
            begin_send_keys_prompt(config, app);
            Ok(false)
        }
        KeyCode::Enter => {
            attach_selected(config, app, terminal, true)?;
            Ok(false)
        }
        KeyCode::Char('a') => {
            attach_selected(config, app, terminal, false)?;
            Ok(false)
        }
        KeyCode::Char('c') => {
            capture_selected(config, app)?;
            Ok(false)
        }
        KeyCode::Char('i') => {
            app.inspect_mode = true;
            spawn_inspect(config, tx, app);
            Ok(false)
        }
        KeyCode::Char('d') => {
            app.show_context = !app.show_context;
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn visible_table_rows(terminal: &Terminal<CrosstermBackend<Stdout>>) -> Result<usize> {
    Ok(visible_table_rows_from_height(terminal.size()?.height))
}

fn visible_table_rows_from_height(terminal_height: u16) -> usize {
    terminal_height.saturating_sub(5).max(1) as usize
}

fn handle_navigation_key(app: &mut App, key: KeyEvent, visible_rows: usize) -> bool {
    let pending_key = app.pending_key.take();
    if pending_key == Some(PendingKey::G)
        && key.code == KeyCode::Char('g')
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        app.select_first_pane();
        app.ensure_selection_visible(visible_rows);
        return true;
    }

    let moved = match key.code {
        KeyCode::Char('g') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.pending_key = Some(PendingKey::G);
            return true;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_selection_by(1);
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_selection_by(-1);
            true
        }
        KeyCode::Home => {
            app.select_first_pane();
            true
        }
        KeyCode::End => {
            app.select_last_pane();
            true
        }
        KeyCode::Char('G') => {
            app.select_last_pane();
            true
        }
        KeyCode::PageUp => {
            app.move_page(-1, visible_rows);
            true
        }
        KeyCode::PageDown => {
            app.move_page(1, visible_rows);
            true
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.move_half_page(-1, visible_rows);
            true
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.move_half_page(1, visible_rows);
            true
        }
        KeyCode::Char('H') => {
            app.select_visible_position(VisiblePosition::Top, visible_rows);
            true
        }
        KeyCode::Char('M') => {
            app.select_visible_position(VisiblePosition::Middle, visible_rows);
            true
        }
        KeyCode::Char('L') => {
            app.select_visible_position(VisiblePosition::Bottom, visible_rows);
            true
        }
        _ => false,
    };
    if moved {
        app.ensure_selection_visible(visible_rows);
    }
    moved
}

fn begin_kill_prompt(config: &Config, app: &mut App) -> Result<()> {
    let Some(row) = app.selected_row() else {
        app.status = "no selected row".to_string();
        return Ok(());
    };
    if let Some(reason) = attach_refusal_reason(row) {
        app.status = format!("selected row cannot be killed: {reason}");
        return Ok(());
    }

    let target = if let Some(watch_id) = &row.watch_id {
        match snapshot::target_for_action(config, watch_id, "kill") {
            Ok(target) => target.to_string(),
            Err(err) => {
                app.status = format!("{err:#}");
                return Ok(());
            }
        }
    } else if let Some(raw_target) = &row.raw_target {
        raw_target.clone()
    } else {
        app.status = "selected row has no live kill target".to_string();
        return Ok(());
    };

    app.kill_prompt = Some(KillPrompt { target });
    Ok(())
}

fn begin_rename_prompt(app: &mut App) {
    let Some(row) = app.selected_row() else {
        app.status = "no selected row".to_string();
        return;
    };
    app.input_prompt = Some(InputPrompt {
        kind: InputPromptKind::RenameSession {
            host: row.host.clone(),
            current: row.tmux.session.clone(),
        },
        value: LineEditor::new(row.tmux.session.clone()),
    });
    app.status = "rename session: edit name and press enter".to_string();
}

fn begin_template_prompt(scoped_host: Option<&str>, app: &mut App) {
    let host = scoped_host
        .map(str::to_string)
        .or_else(|| app.selected_row().map(|row| row.host.clone()))
        .unwrap_or_default();
    app.template_prompt = Some(TemplatePrompt {
        step: TemplatePromptStep::Host {
            value: LineEditor::new(host),
        },
    });
    app.status = "start from template: enter host".to_string();
}

fn begin_send_keys_prompt(config: &Config, app: &mut App) {
    let Some(row) = app.selected_row() else {
        app.status = "no selected row".to_string();
        return;
    };
    let target = if let Some(watch_id) = &row.watch_id {
        match snapshot::target_for_action(config, watch_id, "send keys") {
            Ok(_) => watch_id.clone(),
            Err(err) => {
                app.status = format!("{err:#}");
                return;
            }
        }
    } else if let Some(raw_target) = &row.raw_target {
        raw_target.clone()
    } else {
        app.status = "selected row has no live pane".to_string();
        return;
    };
    app.input_prompt = Some(InputPrompt {
        kind: InputPromptKind::SendKeys {
            target: target.clone(),
        },
        value: LineEditor::default(),
    });
    app.status = format!("send keys to {target}: type command and press enter");
}

fn handle_input_prompt_key(
    key: KeyEvent,
    config: &Config,
    host: Option<String>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let Some(prompt) = app.input_prompt.as_mut() else {
        return Ok(());
    };

    match key.code {
        KeyCode::Esc => {
            app.input_prompt = None;
            app.status = "action cancelled".to_string();
        }
        KeyCode::Enter => {
            execute_input_prompt(config, host, tx, app, terminal)?;
        }
        _ => {
            prompt.value.apply_key(key);
        }
    }
    Ok(())
}

fn handle_template_prompt_key(
    key: KeyEvent,
    config: &Config,
    scoped_host: Option<String>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let Some(prompt) = app.template_prompt.take() else {
        return Ok(());
    };

    match prompt.step {
        TemplatePromptStep::Host { mut value } => match key.code {
            KeyCode::Esc => {
                app.status = "action cancelled".to_string();
            }
            KeyCode::Enter => {
                let host_id = value.as_str().trim().to_string();
                if host_id.is_empty() {
                    app.status = "launch template expects a host".to_string();
                    app.template_prompt = Some(TemplatePrompt {
                        step: TemplatePromptStep::Host { value },
                    });
                    return Ok(());
                }
                if let Some(scope) = scoped_host.as_deref()
                    && scope != host_id
                {
                    app.status = format!(
                        "new session blocked in scoped view: expected host `{scope}`, got `{host_id}`"
                    );
                    app.template_prompt = Some(TemplatePrompt {
                        step: TemplatePromptStep::Host { value },
                    });
                    return Ok(());
                }
                if let Err(err) = config.host(&host_id) {
                    app.status = format!("{err:#}");
                    app.template_prompt = Some(TemplatePrompt {
                        step: TemplatePromptStep::Host { value },
                    });
                    return Ok(());
                }
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Preset {
                        host: host_id.clone(),
                        presets: launch_template_presets(config),
                        selected: 0,
                    },
                });
                app.status = format!("start from template on {host_id}: choose preset");
            }
            _ => {
                value.apply_key(key);
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Host { value },
                });
            }
        },
        TemplatePromptStep::Preset {
            host,
            presets,
            mut selected,
        } => match key.code {
            KeyCode::Esc => {
                app.status = "action cancelled".to_string();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.saturating_sub(1);
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Preset {
                        host,
                        presets,
                        selected,
                    },
                });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if selected + 1 < presets.len() {
                    selected += 1;
                }
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Preset {
                        host,
                        presets,
                        selected,
                    },
                });
            }
            KeyCode::Enter => {
                let Some(preset) = presets.get(selected).cloned() else {
                    app.status = "no launch templates configured".to_string();
                    return Ok(());
                };
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Name {
                        host: host.clone(),
                        preset: preset.clone(),
                        value: LineEditor::default(),
                    },
                });
                app.status = format!("start {} on {host}: enter session name", preset.id);
            }
            _ => {
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Preset {
                        host,
                        presets,
                        selected,
                    },
                });
            }
        },
        TemplatePromptStep::Name {
            host,
            preset,
            mut value,
        } => match key.code {
            KeyCode::Esc => {
                app.status = "action cancelled".to_string();
            }
            KeyCode::Enter => {
                let suffix = value.as_str().trim().to_string();
                if let Err(err) = launch_session_name(&preset, &suffix) {
                    app.status = format!("{err:#}");
                    app.template_prompt = Some(TemplatePrompt {
                        step: TemplatePromptStep::Name {
                            host,
                            preset,
                            value,
                        },
                    });
                    return Ok(());
                }
                start_template_after_directory_selection(
                    config, &host, preset, suffix, tx, app, terminal,
                )?;
            }
            _ => {
                value.apply_key(key);
                app.template_prompt = Some(TemplatePrompt {
                    step: TemplatePromptStep::Name {
                        host,
                        preset,
                        value,
                    },
                });
            }
        },
    }
    Ok(())
}

fn start_template_after_directory_selection(
    config: &Config,
    host_id: &str,
    preset: LaunchTemplatePreset,
    suffix: String,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let selection = suspend_terminal(terminal, || dir_picker::pick_directory(config, host_id));
    match selection {
        Ok(Some(cwd)) => start_template_session(
            config,
            host_id,
            preset,
            &suffix,
            Some(cwd.as_str()),
            tx,
            app,
        ),
        Ok(None) => {
            begin_launch_cwd_prompt(app, host_id, preset, suffix, "directory picker cancelled");
            Ok(())
        }
        Err(err) => {
            begin_launch_cwd_prompt(
                app,
                host_id,
                preset,
                suffix,
                &format!("directory picker unavailable: {err:#}"),
            );
            Ok(())
        }
    }
}

fn begin_launch_cwd_prompt(
    app: &mut App,
    host_id: &str,
    preset: LaunchTemplatePreset,
    suffix: String,
    reason: &str,
) {
    app.input_prompt = Some(InputPrompt {
        kind: InputPromptKind::LaunchTemplateCwd {
            host: host_id.to_string(),
            preset,
            suffix,
        },
        value: LineEditor::default(),
    });
    app.status = format!("{reason}; type cwd or press Enter for default");
}

fn start_template_session(
    config: &Config,
    host_id: &str,
    preset: LaunchTemplatePreset,
    suffix: &str,
    cwd: Option<&str>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
) -> Result<()> {
    match lifecycle::start_launch_template_preset(
        config,
        host_id,
        &preset,
        suffix,
        lifecycle::LaunchTemplateStartOptions {
            cwd,
            window_name: None,
            send_startup_keys: true,
            verbose: false,
        },
    ) {
        Ok(session_name) => {
            app.pending_selection = Some(SelectionPreference::new_session(host_id, &session_name));
            app.status = format!("started {host_id}/{session_name} with {}", preset.id);
            spawn_refresh(config, Some(host_id.to_string()), tx, app)?;
        }
        Err(err) => app.status = format!("{err:#}"),
    }
    Ok(())
}

fn execute_input_prompt(
    config: &Config,
    scoped_host: Option<String>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let Some(prompt) = app.input_prompt.take() else {
        return Ok(());
    };

    match prompt.kind {
        InputPromptKind::RenameSession {
            host: host_id,
            current,
        } => {
            let new_name = prompt.value.as_str().trim();
            if new_name.is_empty() {
                app.status = "rename cancelled: empty name".to_string();
                return Ok(());
            }
            if new_name == current {
                app.status = "rename skipped: unchanged".to_string();
                return Ok(());
            }
            match lifecycle::rename_session(config, &host_id, &current, new_name, false) {
                Ok(()) => {
                    app.status = format!("renamed {host_id}/{current} -> {new_name}");
                    spawn_refresh(config, Some(host_id), tx, app)?;
                }
                Err(err) => app.status = format!("{err:#}"),
            }
        }
        InputPromptKind::NewSession => {
            let Some((host_id, session_name)) = parse_host_session(prompt.value.as_str().trim())
            else {
                app.status = "new session expects <host>/<session>".to_string();
                return Ok(());
            };
            if let Some(scope) = scoped_host.as_deref()
                && scope != host_id
            {
                app.status = format!(
                    "new session blocked in scoped view: expected host `{scope}`, got `{host_id}`"
                );
                return Ok(());
            }
            create_session_after_directory_selection(
                config,
                host_id,
                session_name,
                tx,
                app,
                terminal,
            )?;
        }
        InputPromptKind::NewSessionCwd { host, session } => {
            let cwd = prompt.value.as_str().trim();
            let cwd = if cwd.is_empty() { None } else { Some(cwd) };
            create_session(config, &host, &session, cwd, tx, app)?;
        }
        InputPromptKind::LaunchTemplateCwd {
            host,
            preset,
            suffix,
        } => {
            let cwd = prompt.value.as_str().trim();
            let cwd = if cwd.is_empty() { None } else { Some(cwd) };
            start_template_session(config, &host, preset, &suffix, cwd, tx, app)?;
        }
        InputPromptKind::NewPane => {
            let Some((host_id, session_name)) = parse_host_session(prompt.value.as_str().trim())
            else {
                app.status = "new pane expects <host>/<session>".to_string();
                return Ok(());
            };
            if let Some(scope) = scoped_host.as_deref()
                && scope != host_id
            {
                app.status = format!(
                    "new pane blocked in scoped view: expected host `{scope}`, got `{host_id}`"
                );
                return Ok(());
            }
            match lifecycle::new_pane(config, host_id, session_name, false) {
                Ok(()) => {
                    app.status = format!("spawned pane in {host_id}/{session_name}");
                    spawn_refresh(config, Some(host_id.to_string()), tx, app)?;
                }
                Err(err) => app.status = format!("{err:#}"),
            }
        }
        InputPromptKind::SendKeys { target } => {
            let keys = prompt.value.into_string();
            if keys.is_empty() {
                app.status = "send keys cancelled: empty input".to_string();
                return Ok(());
            }
            match lifecycle::send_keys(config, &target, &keys, true, false) {
                Ok(()) => {
                    app.status = format!("sent keys to {target}");
                    spawn_refresh(config, scoped_host, tx, app)?;
                }
                Err(err) => app.status = format!("{err:#}"),
            }
        }
    }
    Ok(())
}

fn create_session_after_directory_selection(
    config: &Config,
    host_id: &str,
    session_name: &str,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    let selection = suspend_terminal(terminal, || dir_picker::pick_directory(config, host_id));
    match selection {
        Ok(Some(cwd)) => create_session(config, host_id, session_name, Some(cwd.as_str()), tx, app),
        Ok(None) => {
            begin_cwd_prompt(app, host_id, session_name, "directory picker cancelled");
            Ok(())
        }
        Err(err) => {
            begin_cwd_prompt(
                app,
                host_id,
                session_name,
                &format!("directory picker unavailable: {err:#}"),
            );
            Ok(())
        }
    }
}

fn begin_cwd_prompt(app: &mut App, host_id: &str, session_name: &str, reason: &str) {
    app.input_prompt = Some(InputPrompt {
        kind: InputPromptKind::NewSessionCwd {
            host: host_id.to_string(),
            session: session_name.to_string(),
        },
        value: LineEditor::default(),
    });
    app.status = format!("{reason}; type cwd or press Enter for default");
}

fn create_session(
    config: &Config,
    host_id: &str,
    session_name: &str,
    cwd: Option<&str>,
    tx: mpsc::Sender<RefreshMessage>,
    app: &mut App,
) -> Result<()> {
    match lifecycle::new_session(config, host_id, session_name, cwd, None, false) {
        Ok(()) => {
            let cwd_label = cwd.map(|cwd| format!(" in {cwd}")).unwrap_or_default();
            app.status = format!("created session {host_id}/{session_name}{cwd_label}");
            app.pending_selection = Some(SelectionPreference::new_session(host_id, session_name));
            spawn_refresh(config, Some(host_id.to_string()), tx, app)?;
        }
        Err(err) => app.status = format!("{err:#}"),
    }
    Ok(())
}

fn parse_host_session(input: &str) -> Option<(&str, &str)> {
    let (host, session) = input.split_once('/')?;
    if host.is_empty() || session.is_empty() || session.contains(':') {
        return None;
    }
    Some((host, session))
}

fn attach_selected(
    config: &Config,
    app: &mut App,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    readonly: bool,
) -> Result<()> {
    if app.selected_row().is_none() {
        app.status = "no selected row".to_string();
        return Ok(());
    }
    let action = if readonly { "read-only attach" } else { "jump" };
    let result = suspend_terminal(terminal, || -> Result<PaneTarget> {
        let target = selected_attach_target(config, app, action)?;
        attach::attach_target(config, &target, readonly)
            .with_context(|| format!("failed to {action} `{target}`"))?;
        Ok(target)
    });
    match result {
        Ok(target) => app.status = format!("{action}ed {target}"),
        Err(err) => app.status = format!("{err:#}"),
    }
    Ok(())
}

fn capture_selected(config: &Config, app: &mut App) -> Result<()> {
    let Some(row) = app.selected_row() else {
        app.status = "no selected row".to_string();
        return Ok(());
    };
    let Some(raw_target) = &row.raw_target else {
        app.status = "selected row has no live pane".to_string();
        return Ok(());
    };
    let display_id = row.display_id.clone();
    let target = PaneTarget::parse(raw_target)?;
    match snapshot::capture_pane(config, &target, config.poll.capture_lines, false) {
        Ok(output) => {
            app.captured_for = Some(display_id);
            app.captured_output = Some(output);
            app.status = format!("captured {target}");
        }
        Err(err) => app.status = format!("{err:#}"),
    }
    Ok(())
}

fn selected_attach_target(config: &Config, app: &App, action: &str) -> Result<PaneTarget> {
    let row = app
        .selected_row()
        .ok_or_else(|| anyhow!("no selected row"))?;
    if let Some(reason) = attach_refusal_reason(row) {
        bail!("selected row cannot attach: {reason}");
    }
    if let Some(watch_id) = &row.watch_id {
        return snapshot::target_for_action(config, watch_id, action);
    }

    let raw_target = row
        .raw_target
        .as_ref()
        .ok_or_else(|| anyhow!("selected row has no live pane"))?;
    PaneTarget::parse(raw_target)
        .with_context(|| format!("selected row has invalid pane target `{raw_target}`"))
}

fn attach_refusal_reason(row: &SessionSnapshot) -> Option<String> {
    match row.match_status {
        MatchStatus::Matched | MatchStatus::Orphan => None,
        MatchStatus::Missing => Some("missing live pane".to_string()),
        MatchStatus::Ambiguous => Some("ambiguous candidates".to_string()),
        MatchStatus::Shadowed => Some(format!(
            "shadowed by {}",
            row.shadowed_by.as_deref().unwrap_or("another watch")
        )),
        MatchStatus::Unreachable => Some("host unreachable".to_string()),
        MatchStatus::Unknown => Some("unknown attach state".to_string()),
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(area);

    draw_summary(frame, chunks[0], app);
    if app.inspect_mode || app.help {
        draw_inspector(frame, chunks[1], app);
    } else if app.show_context && area.width >= 96 {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(chunks[1]);
        draw_live_table(frame, body[0], app);
        draw_context_rail(frame, body[1], app);
    } else {
        draw_live_table(frame, chunks[1], app);
    }
    draw_status(frame, chunks[2], app);
}

fn draw_summary(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let rows = app.rows();
    let total = rows.len();
    let watched = rows
        .iter()
        .filter(|r| r.match_status == MatchStatus::Matched)
        .count();
    let free = rows
        .iter()
        .filter(|r| r.match_status == MatchStatus::Orphan)
        .count();
    let problems = rows
        .iter()
        .filter(|r| {
            matches!(
                r.match_status,
                MatchStatus::Ambiguous
                    | MatchStatus::Shadowed
                    | MatchStatus::Missing
                    | MatchStatus::Unreachable
                    | MatchStatus::Unknown
            )
        })
        .count();

    // Host scan status: "N/N hosts Xs"
    let host_total = app.host_progress.len();
    let host_done = app
        .host_progress
        .iter()
        .filter(|h| h.state.is_finished())
        .count();
    let elapsed_str = app.refresh_status_str().unwrap_or_else(|| "-".to_string());

    let mode = if app.inspect_mode {
        "inspect"
    } else if app.editing_filter {
        "filter"
    } else {
        "browse"
    };

    let compact = area.width < 110;
    let mut spans = vec![Span::styled(
        "remux",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(if compact { " " } else { " | " }));
    spans.push(Span::raw("/ "));
    if app.editing_filter {
        spans.extend(app.filter.cursor_spans());
    } else {
        spans.push(Span::raw(filter_label(app)));
    }
    if compact {
        spans.push(Span::raw(format!(" | {total} panes")));
        if problems > 0 {
            spans.push(Span::styled(
                format!(" !{problems}"),
                Style::default().fg(Color::Red),
            ));
        }
        spans.push(Span::raw(format!(
            " | {host_done}/{host_total} hosts {elapsed_str} | "
        )));
        spans.push(Span::styled(mode, Style::default().fg(Color::Cyan)));
    } else {
        spans.extend([
            Span::raw(" | "),
            Span::raw(format!("{total} panes  ")),
            Span::styled(format!("•{free} free"), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled(
                format!("◆{watched} watched"),
                Style::default().fg(Color::Cyan),
            ),
        ]);
        if problems > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("!{problems} issues"),
                Style::default().fg(Color::Red),
            ));
        }
        spans.push(Span::raw(format!(
            " | {host_done}/{host_total} hosts {elapsed_str}"
        )));
        spans.push(Span::raw(" | "));
        spans.push(Span::styled(mode, Style::default().fg(Color::Cyan)));
        spans.push(Span::raw(" | sort "));
        spans.push(Span::styled(
            sort_label(app.sort_field, app.sort_direction),
            muted_style(),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn row_glyph(session: &SessionSnapshot) -> &'static str {
    match session.match_status {
        MatchStatus::Matched => "◆ ",
        MatchStatus::Orphan => "• ",
        _ => "! ",
    }
}

/// Build a "[window-name | pane-title]" suffix for the live table row.
///
/// We only show pieces that are informative — tmux fills these fields with
/// defaults (window-name == command, pane-title == host_short) that just
/// repeat info already on the row, so we drop them.
fn friendly_label_suffix(session: &SessionSnapshot) -> Option<String> {
    let cmd = session.process.as_ref().map(|p| p.command.as_str());
    let host_short = session.tmux.host_short.as_deref();
    let window_idx = session.tmux.window.as_deref();

    let window_name = session
        .tmux
        .window_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| Some(*name) != cmd)
        .filter(|name| Some(*name) != window_idx);

    let pane_title = session
        .tmux
        .pane_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .filter(|title| !is_default_host_title(title, host_short))
        .filter(|title| Some(*title) != cmd)
        .filter(|title| Some(*title) != window_name);

    match (window_name, pane_title) {
        (None, None) => None,
        (Some(w), None) => Some(format!("[{w}]")),
        (None, Some(t)) => Some(format!("[{t}]")),
        (Some(w), Some(t)) => Some(format!("[{w} | {t}]")),
    }
}

/// Tmux's default `pane_title` is the remote hostname (often the FQDN even
/// though `#{host_short}` is just the short form). Treat any title that
/// equals the short host or has it as a leading dotted segment (the FQDN
/// case, plus the common `user@host:cwd` shell prompt) as a default.
fn is_default_host_title(title: &str, host_short: Option<&str>) -> bool {
    let Some(host) = host_short.filter(|s| !s.is_empty()) else {
        return false;
    };
    if title == host {
        return true;
    }
    if let Some(rest) = title.strip_prefix(host) {
        // FQDN: "host.us-west-2.amazon.com"
        if rest.starts_with('.') {
            return true;
        }
    }
    if let Some((_, after_at)) = title.split_once('@') {
        // "user@host:~/path" — strip the user, then check the host portion.
        let host_part = after_at.split(':').next().unwrap_or(after_at);
        if host_part == host
            || host_part
                .strip_prefix(host)
                .is_some_and(|r| r.starts_with('.'))
        {
            return true;
        }
    }
    false
}

enum LiveTableRow<'a> {
    Group {
        label: String,
        depth: u8,
    },
    Pane {
        row: &'a SessionSnapshot,
        pane_index: usize,
    },
}

fn live_table_rows<'a>(rows: &[&'a SessionSnapshot]) -> Vec<LiveTableRow<'a>> {
    let mut table_rows = Vec::new();
    let mut current_host: Option<(String, Option<String>)> = None;
    let mut current_container: Option<String> = None;
    let mut current_session: Option<String> = None;
    let mut current_window: Option<String> = None;

    for (pane_index, row) in rows.iter().enumerate() {
        let host_key = (row.host.clone(), row.tmux_socket.clone());
        if current_host.as_ref() != Some(&host_key) {
            table_rows.push(LiveTableRow::Group {
                label: host_group_label(row),
                depth: 0,
            });
            current_host = Some(host_key);
            current_container = None;
            current_session = None;
            current_window = None;
        }

        if current_container.as_deref() != row.container.as_deref() {
            if let Some(container) = &row.container {
                table_rows.push(LiveTableRow::Group {
                    label: format!("container {container}"),
                    depth: 1,
                });
            }
            current_container = row.container.clone();
            current_session = None;
            current_window = None;
        }

        if current_session.as_deref() != Some(row.tmux.session.as_str()) {
            table_rows.push(LiveTableRow::Group {
                label: format!("session {}", row.tmux.session),
                depth: 2,
            });
            current_session = Some(row.tmux.session.clone());
            current_window = None;
        }

        if let Some(window) = &row.tmux.window
            && current_window.as_deref() != Some(window.as_str())
        {
            table_rows.push(LiveTableRow::Group {
                label: window_group_label(row),
                depth: 3,
            });
            current_window = Some(window.clone());
        }

        table_rows.push(LiveTableRow::Pane { row, pane_index });
    }

    table_rows
}

fn display_index_for_pane(display_rows: &[LiveTableRow<'_>], selected: usize) -> Option<usize> {
    display_rows.iter().position(
        |row| matches!(row, LiveTableRow::Pane { pane_index, .. } if *pane_index == selected),
    )
}

fn nearest_pane_index_for_display_row(
    display_rows: &[LiveTableRow<'_>],
    target: usize,
) -> Option<usize> {
    nearest_pane_index_for_display_row_with_tie(display_rows, target, PaneTieBreak::After)
}

fn nearest_pane_index_for_directional_display_row(
    display_rows: &[LiveTableRow<'_>],
    target: usize,
    delta: isize,
) -> Option<usize> {
    let tie_break = if delta < 0 {
        PaneTieBreak::Before
    } else {
        PaneTieBreak::After
    };
    nearest_pane_index_for_display_row_with_tie(display_rows, target, tie_break)
}

#[derive(Clone, Copy)]
enum PaneTieBreak {
    Before,
    After,
}

fn nearest_pane_index_for_display_row_with_tie(
    display_rows: &[LiveTableRow<'_>],
    target: usize,
    tie_break: PaneTieBreak,
) -> Option<usize> {
    display_rows
        .iter()
        .enumerate()
        .filter_map(|(display_index, row)| match row {
            LiveTableRow::Pane { pane_index, .. } => Some((display_index, *pane_index)),
            LiveTableRow::Group { .. } => None,
        })
        .min_by_key(|(display_index, _)| {
            let tie_rank = match tie_break {
                PaneTieBreak::Before => usize::from(*display_index > target),
                PaneTieBreak::After => usize::from(*display_index < target),
            };
            (display_index.abs_diff(target), tie_rank)
        })
        .map(|(_, pane_index)| pane_index)
}

fn pane_index_for_visible_position(
    display_rows: &[LiveTableRow<'_>],
    start: usize,
    end: usize,
    target: usize,
    position: VisiblePosition,
) -> Option<usize> {
    let mut visible_panes = display_rows
        .iter()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start) + 1)
        .filter_map(|(display_index, row)| match row {
            LiveTableRow::Pane { pane_index, .. } => Some((display_index, *pane_index)),
            LiveTableRow::Group { .. } => None,
        });

    match position {
        VisiblePosition::Top => visible_panes.next().map(|(_, pane_index)| pane_index),
        VisiblePosition::Middle => visible_panes
            .min_by_key(|(display_index, _)| display_index.abs_diff(target))
            .map(|(_, pane_index)| pane_index),
        VisiblePosition::Bottom => visible_panes.next_back().map(|(_, pane_index)| pane_index),
    }
}

fn host_group_label(row: &SessionSnapshot) -> String {
    match row.tmux_socket.as_deref() {
        Some(socket) => format!("host {}  [{}]", row.host, socket),
        None => format!("host {}", row.host),
    }
}

fn window_group_label(row: &SessionSnapshot) -> String {
    let window = row.tmux.window.as_deref().unwrap_or("-");
    let window_name = row
        .tmux
        .window_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| *name != window);
    match window_name {
        Some(name) => format!("window {window}  [{name}]"),
        None => format!("window {window}"),
    }
}

fn draw_live_table(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let rows = app.rows();
    if rows.is_empty() {
        let paragraph = Paragraph::new(Text::from(empty_state_lines(app)))
            .block(Block::default().borders(Borders::ALL).title("live tree"))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
        return;
    }

    let preview_min: u16 = 20;

    let selected_idx = app.selected.min(rows.len() - 1);
    let display_rows = live_table_rows(&rows);
    let selected_display_idx = display_rows
        .iter()
        .position(|row| matches!(row, LiveTableRow::Pane { pane_index, .. } if *pane_index == selected_idx))
        .unwrap_or(0);
    let table_rows: Vec<Row> = display_rows
        .iter()
        .map(|display_row| match display_row {
            LiveTableRow::Group { label, depth } => group_table_row(label, *depth),
            LiveTableRow::Pane { row, pane_index } => {
                pane_table_row(row, *pane_index == selected_idx)
            }
        })
        .collect();

    let table = Table::new(
        table_rows,
        [
            Constraint::Min(20),
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Min(preview_min),
        ],
    )
    .header(
        Row::new(["NAME", "AGE", "CMD", "PREVIEW"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title("live tree"))
    .row_highlight_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    );

    let mut state = TableState::default().with_offset(app.table_offset);
    state.select(Some(selected_display_idx));
    frame.render_stateful_widget(table, area, &mut state);
}

fn group_table_row(label: &str, depth: u8) -> Row<'static> {
    let indent = "  ".repeat(depth as usize);
    let name = Line::from(vec![
        Span::raw(indent),
        Span::styled("▾ ", muted_style()),
        Span::styled(label.to_string(), label_style()),
    ]);
    Row::new(vec![
        Cell::from(name),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
    ])
    .style(muted_style())
}

fn pane_table_row(session: &SessionSnapshot, selected: bool) -> Row<'static> {
    let selector = if selected { "› " } else { "  " };
    let glyph = row_glyph(session);
    let indent = if session.tmux.window.is_some() {
        "      "
    } else {
        "    "
    };
    let prefix = format!("{indent}{selector}{glyph}");

    let mut name_spans = vec![Span::styled(prefix, canonical_state_style(session))];
    if session.tmux_socket.is_some() {
        name_spans.push(Span::styled(
            "i ",
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD),
        ));
    }
    name_spans.push(Span::styled(
        session.display_id.clone(),
        canonical_state_style(session),
    ));
    if let Some(label) = friendly_label_suffix(session) {
        name_spans.push(Span::raw(" "));
        name_spans.push(Span::styled(label, muted_style()));
    }
    let name_cell = Cell::from(Line::from(name_spans));

    let age_cell = Cell::from(last_output_age(session));

    let cmd_cell = Cell::from(session.display_command().unwrap_or_else(|| "-".to_string()));

    let preview_text = session
        .output
        .as_ref()
        .and_then(|o| {
            let src = if o.recent.is_empty() {
                &o.preview
            } else {
                &o.recent
            };
            src.lines()
                .rfind(|l| !l.trim().is_empty())
                .map(|l| l.to_string())
        })
        .or_else(|| row_issue_summary(session))
        .unwrap_or_else(|| "(no capture)".to_string());
    let preview_cell = Cell::from(preview_text).style(muted_style());

    Row::new(vec![name_cell, age_cell, cmd_cell, preview_cell])
}

fn draw_inspector(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let block =
        Block::default()
            .borders(Borders::ALL)
            .title(if app.help { "keys" } else { "inspector" });
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.help {
        let text = Text::from(help_lines(inner.width));
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
        return;
    }

    let Some(selected) = app.selected_row() else {
        frame.render_widget(
            Paragraph::new(Text::from(empty_state_lines(app))).wrap(Wrap { trim: false }),
            inner,
        );
        return;
    };

    let row = inspected_row(app, selected);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Text::from(detail_meta_lines(row))).wrap(Wrap { trim: false }),
        chunks[0],
    );

    // Right column: status notes pinned on top (fixed size based on content),
    // preview fills the rest. Without this split a long preview would push
    // status notes off-screen.
    let right = chunks[1];
    let status_lines = detail_status_lines(row);
    let status_height = (status_lines.len() as u16).min(right.height.saturating_sub(3));
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(status_height), Constraint::Min(1)])
        .split(right);

    if status_height > 0 {
        frame.render_widget(
            Paragraph::new(Text::from(status_lines)).wrap(Wrap { trim: false }),
            right_chunks[0],
        );
    }

    let preview_area = right_chunks[1];
    // Header + trailing tail lines sized to actually fit the preview area.
    let preview_body_rows = preview_area.height.saturating_sub(1) as usize;
    frame.render_widget(
        Paragraph::new(Text::from(detail_preview_lines(
            app,
            selected,
            row,
            preview_body_rows,
        )))
        .wrap(Wrap { trim: false }),
        preview_area,
    );
}

fn draw_status(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    if let Some(prompt) = &app.kill_prompt {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("confirm ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("kill {}? (y/N)", prompt.target)),
            ])),
            area,
        );
        return;
    }
    if let Some(prompt) = &app.input_prompt {
        frame.render_widget(Paragraph::new(input_prompt_line(prompt)), area);
        return;
    }
    if let Some(prompt) = &app.template_prompt {
        frame.render_widget(Paragraph::new(template_prompt_line(prompt)), area);
        return;
    }

    let mode = if app.editing_filter {
        "filter"
    } else if app.is_polling() {
        "polling"
    } else {
        "ready"
    };
    let spans = vec![
        Span::raw(status_hint(area.width)),
        Span::styled(mode, Style::default().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(short_message(&app.status), muted_style()),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn help_lines(width: u16) -> Vec<Line<'static>> {
    const HELP: &[&str] = &[
        "[j/↓] select next",
        "[k/↑] select previous",
        "[Home/End] first/last pane",
        "[gg/G] first/last pane",
        "[PgUp/PgDn] page",
        "[Ctrl-u/Ctrl-d] half page",
        "[H/M/L] screen top/middle/bottom",
        "[r] refresh now",
        "[s] cycle sort field",
        "[S] toggle sort direction",
        "[/] filter",
        "[Enter] readonly attach",
        "[a] read-write jump",
        "[c] capture detail",
        "[i] inspect and refresh",
        "[x] kill selected pane",
        "[e] rename session",
        "[n] create session",
        "[t] start from template",
        "[p] spawn pane",
        "[z] send keys",
        "[d] toggle detail pane",
        "[?] toggle help",
        "[q] quit",
    ];

    if width < 72 {
        return HELP.iter().map(|item| Line::from(*item)).collect();
    }

    let split = HELP.len().div_ceil(2);
    (0..split)
        .map(|index| {
            let left = HELP[index];
            let right = HELP.get(index + split).copied().unwrap_or("");
            Line::from(format!("{left:<34}{right}"))
        })
        .collect()
}

fn status_hint(width: u16) -> &'static str {
    if width < 110 {
        "[↑↓] move  [Enter/a] attach  [i] inspect  [/] filter  [?] help  [q] quit   "
    } else {
        "[↑↓ PgUp/PgDn] move  [gg/G] ends  [Enter/a] attach  [s/S] sort  [t] start  [z] send  [i] inspect  [/] filter  [d] details  [?] help  [q] quit   "
    }
}

fn draw_context_rail(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let Some(selected) = app.selected_row() else {
        return;
    };

    let lines = context_rail_lines(app, selected, area.height);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().title("context").borders(Borders::LEFT)),
        area,
    );
}

fn context_rail_lines(
    app: &App,
    selected: &SessionSnapshot,
    available_height: u16,
) -> Vec<Line<'static>> {
    // Action line: spinner while pending, otherwise hotkey hints.
    let is_pending = app.inspect_pending.as_deref() == Some(selected.display_id.as_str());
    let action_line = if is_pending {
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_millis())
            .unwrap_or(0);
        let frame_idx = (millis / 100) as usize % SPINNER.len();
        Line::from(Span::styled(
            format!("{} refreshing…", SPINNER[frame_idx]),
            muted_style(),
        ))
    } else {
        Line::from(Span::styled(
            "[i] refresh · [Enter] attach ro · [a] jump rw · [t] start · [z] send keys · [c] capture",
            muted_style(),
        ))
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("target ", label_style()),
            Span::raw(table_target(selected)),
        ]),
        Line::from(vec![
            Span::styled("host ", label_style()),
            Span::raw(selected.host.clone()),
        ]),
        Line::from(vec![
            Span::styled("socket ", label_style()),
            Span::raw(
                selected
                    .tmux_socket
                    .as_deref()
                    .unwrap_or("default")
                    .to_string(),
            ),
        ]),
        Line::from(vec![
            Span::styled("state ", label_style()),
            Span::styled(canonical_state(selected), canonical_state_style(selected)),
        ]),
        Line::from(vec![
            Span::styled("match ", label_style()),
            Span::styled(
                selected.match_status.as_str(),
                match_style(selected.match_status),
            ),
            Span::raw("  "),
            Span::styled("expected ", label_style()),
            Span::raw(selected.target.clone()),
        ]),
        Line::from(vec![
            Span::styled("cmd ", label_style()),
            Span::raw(
                selected
                    .process
                    .as_ref()
                    .map(|p| short_message(&p.command))
                    .unwrap_or_else(|| "-".into()),
            ),
        ]),
        Line::from(""),
    ];
    lines.extend(context_project_lines(selected));
    if selected.repo.is_some() {
        lines.push(Line::from(""));
    }

    // Reserve one blank separator plus the action line. If fixed metadata
    // consumes nearly all of a short rail, trim it before budgeting output so
    // the bottom action hint remains visible.
    let reserved_tail = 2usize;
    let available_height = available_height as usize;
    let max_fixed_lines = available_height.saturating_sub(reserved_tail + 1);
    if lines.len() > max_fixed_lines {
        lines.truncate(max_fixed_lines);
        lines.push(Line::from(Span::styled("...", muted_style())));
    }
    let output_budget = available_height
        .saturating_sub(lines.len() + reserved_tail)
        .max(1);

    // Cached output section.
    let output_lines: Vec<Line<'static>> = if let Some(output) = selected.output.as_ref() {
        let age = {
            let age_str = if let Some(ts) = output.last_output_at {
                let secs = Utc::now().signed_duration_since(ts).num_seconds().max(0);
                if secs < 60 {
                    format!("{secs}s ago")
                } else if secs < 3600 {
                    format!("{}m ago", secs / 60)
                } else {
                    format!("{}h ago", secs / 3600)
                }
            } else {
                "unknown age".to_string()
            };
            Line::from(Span::styled(format!("captured {age_str}"), muted_style()))
        };
        let text = if output.recent.is_empty() {
            output.preview.as_str()
        } else {
            output.recent.as_str()
        };
        let max_preview = output_budget.saturating_sub(1);
        let mut lines: Vec<Line<'static>> = vec![age];
        for line in tail_lines(text, max_preview) {
            lines.push(Line::from(line));
        }
        lines
    } else {
        vec![Line::from(Span::styled(
            "no capture yet — press [i] to fetch",
            muted_style(),
        ))]
    };

    lines.extend(output_lines);
    lines.push(Line::from(""));
    lines.push(action_line);
    lines
}

fn canonical_state(row: &SessionSnapshot) -> &'static str {
    if matches!(row.match_status, MatchStatus::Ambiguous) {
        return "ambiguous";
    }
    if matches!(
        row.match_status,
        MatchStatus::Missing | MatchStatus::Unreachable
    ) {
        return "missing";
    }
    // drift: only when a configured watch's pane identity shifted (Shadowed)
    if matches!(row.match_status, MatchStatus::Shadowed) {
        return "drift";
    }
    // Orphan, Matched, Unknown: classify by session state
    match row.state {
        SessionState::Active => "ready",
        SessionState::Idle => "stale",
        SessionState::Quiet => "busy",
        SessionState::Missing => "missing",
        SessionState::Unreachable => "missing",
        // Unknown state with no other signal: neutral '-' rather than 'drift'
        SessionState::Unknown => "-",
    }
}
fn canonical_state_style(row: &SessionSnapshot) -> Style {
    match canonical_state(row) {
        "ready" => Style::default().fg(Color::Green),
        "stale" => Style::default().fg(Color::Yellow),
        "drift" => Style::default().fg(Color::LightBlue),
        "busy" => Style::default().fg(Color::LightYellow),
        "missing" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        "ambiguous" => Style::default().fg(Color::Magenta),
        "-" => Style::default().fg(Color::DarkGray),
        _ => Style::default(),
    }
}

fn inspected_row<'a>(app: &'a App, selected: &'a SessionSnapshot) -> &'a SessionSnapshot {
    if app.inspected_for.as_deref() == Some(selected.display_id.as_str())
        && let Some(detail) = &app.inspected_detail
    {
        return &detail.session;
    }
    selected
}

fn input_prompt_line(prompt: &InputPrompt) -> Line<'static> {
    let (label, hint) = match &prompt.kind {
        InputPromptKind::RenameSession { host, current } => (
            format!("rename {host}/{current} -> "),
            "enter new session name (Esc to cancel)",
        ),
        InputPromptKind::NewSession => (
            "new session ".to_string(),
            "enter <host>/<session> (Esc to cancel)",
        ),
        InputPromptKind::NewSessionCwd { host, session } => (
            format!("cwd for {host}/{session} "),
            "optional path; empty uses tmux default (Esc to cancel)",
        ),
        InputPromptKind::LaunchTemplateCwd {
            host,
            preset,
            suffix,
        } => (
            format!(
                "cwd for {host}/{} ",
                launch_session_name(preset, suffix).unwrap_or_else(|_| suffix.clone())
            ),
            "optional path; Enter starts template (Esc to cancel)",
        ),
        InputPromptKind::NewPane => (
            "new pane ".to_string(),
            "enter <host>/<session> (Esc to cancel)",
        ),
        InputPromptKind::SendKeys { target } => (
            format!("send {target} "),
            "literal text; Enter sends text plus Enter (Esc to cancel)",
        ),
    };
    editable_prompt_line(label, &prompt.value, hint)
}

fn template_prompt_line(prompt: &TemplatePrompt) -> Line<'static> {
    match &prompt.step {
        TemplatePromptStep::Host { value } => editable_prompt_line(
            "template host ",
            value,
            "Enter chooses host (Esc to cancel)",
        ),
        TemplatePromptStep::Preset {
            host,
            presets,
            selected,
        } => {
            let selected_preset = presets
                .get(*selected)
                .map(launch_template_label)
                .unwrap_or_else(|| "-".to_string());
            Line::from(vec![
                Span::styled(
                    format!("template {host} "),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(selected_preset),
                Span::raw(" | "),
                Span::styled(
                    "[j/k] choose prefix, Enter selects (Esc to cancel)",
                    muted_style(),
                ),
            ])
        }
        TemplatePromptStep::Name {
            host,
            preset,
            value,
        } => editable_prompt_line(
            format!("template {host}/{}-", preset.session_prefix),
            value,
            "Enter starts template (Esc to cancel)",
        ),
    }
}

fn editable_prompt_line(
    label: impl Into<String>,
    value: &LineEditor,
    hint: &'static str,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        label.into(),
        Style::default().fg(Color::Yellow),
    )];
    spans.extend(value.cursor_spans());
    spans.extend([Span::raw(" | "), Span::styled(hint, muted_style())]);
    Line::from(spans)
}

fn detail_meta_lines(row: &SessionSnapshot) -> Vec<Line<'static>> {
    let pid = row
        .process
        .as_ref()
        .and_then(|process| process.pid)
        .map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_string());
    let command = row
        .process
        .as_ref()
        .map(|process| process.command.as_str())
        .unwrap_or("-");
    let cwd = row
        .process
        .as_ref()
        .map(|process| process.cwd.as_str())
        .unwrap_or("-");
    let repo_path = row
        .repo
        .as_ref()
        .map(|repo| repo.path.as_str())
        .unwrap_or("-");
    let branch = row
        .repo
        .as_ref()
        .and_then(|repo| repo.branch.as_deref())
        .unwrap_or("-");
    let dirty = row
        .repo
        .as_ref()
        .and_then(|repo| repo.dirty_count)
        .map(|dirty| dirty.to_string())
        .unwrap_or_else(|| "-".to_string());
    let watch = row.watch_id.as_deref().unwrap_or("-");
    let raw_target = row.raw_target.as_deref().unwrap_or("-");
    let socket = row.tmux_socket.as_deref().unwrap_or("default");

    let mut lines = vec![
        Line::from(Span::styled(
            row.display_id.clone(),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("activity: ", label_style()),
            Span::styled(row.state.as_str(), state_style(row.state)),
            Span::raw(" | "),
            Span::styled("match: ", label_style()),
            Span::styled(row.match_status.as_str(), match_style(row.match_status)),
            Span::raw(" | "),
            Span::styled("watch: ", label_style()),
            Span::raw(watch.to_string()),
        ]),
        Line::from(vec![
            Span::styled("target: ", label_style()),
            Span::raw(raw_target.to_string()),
        ]),
        Line::from(vec![
            Span::styled("socket: ", label_style()),
            Span::raw(socket.to_string()),
        ]),
        Line::from(vec![
            Span::styled("tmux: ", label_style()),
            Span::raw(tmux_target(row)),
            Span::raw(" | "),
            Span::styled("pane id: ", label_style()),
            Span::raw(row.tmux.pane_id.as_deref().unwrap_or("-").to_string()),
            Span::raw(" | "),
            Span::styled("pid: ", label_style()),
            Span::raw(pid.clone()),
        ]),
    ];

    if let Some(label) = friendly_label_suffix(row) {
        lines.push(Line::from(vec![
            Span::styled("label: ", label_style()),
            Span::raw(label.trim_matches(|c| c == '[' || c == ']').to_string()),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::styled("command: ", label_style()),
            Span::raw(command.to_string()),
        ]),
        Line::from(vec![
            Span::styled("cwd: ", label_style()),
            Span::raw(cwd.to_string()),
        ]),
        Line::from(vec![
            Span::styled("repo: ", label_style()),
            Span::raw(repo_path.to_string()),
        ]),
        Line::from(vec![
            Span::styled("branch: ", label_style()),
            Span::raw(branch.to_string()),
            Span::raw(" | "),
            Span::styled("dirty: ", label_style()),
            Span::styled(dirty.clone(), dirty_style(dirty.as_str())),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("attach: ", label_style()),
            Span::raw(attach_hint(row)),
        ]),
        Line::from(vec![
            Span::styled("capture: ", label_style()),
            Span::raw(capture_hint(row)),
        ]),
    ]);
    if let Some(agent) = &row.agent_hint {
        lines.push(Line::from(vec![
            Span::styled("hint: ", label_style()),
            Span::raw(agent.clone()),
        ]));
    }
    lines
}

fn detail_preview_lines(
    app: &App,
    selected: &SessionSnapshot,
    row: &SessionSnapshot,
    max_body_lines: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "Recent output preview",
        Style::default().add_modifier(Modifier::BOLD),
    ))];

    if let Some(output) = selected_output(app, selected, row).filter(|output| !output.is_empty()) {
        // Emit exactly as many tail lines as will fit in the preview pane so
        // the most recent output is visible rather than clipped off-screen.
        let tail_count = max_body_lines.max(1);
        for line in tail_lines(output, tail_count) {
            lines.push(Line::from(line));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No recent output in cache. Capture or inspect the selected pane to refresh this view.",
            muted_style(),
        )));
    }

    lines
}

fn detail_status_lines(row: &SessionSnapshot) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(summary) = row_issue_summary(row) {
        lines.push(Line::from(vec![
            Span::styled("Issue: ", Style::default().fg(Color::Red)),
            Span::raw(summary),
        ]));
    }

    if let Some(repo) = &row.repo {
        if !repo.changed_files.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("Changed files ({})", repo.changed_files.len()),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for file in repo.changed_files.iter().take(6) {
                lines.push(Line::from(file.clone()));
            }
            if repo.changed_files.len() > 6 {
                lines.push(Line::from(Span::styled("...", muted_style())));
            }
        }
        if let Some(error) = &repo.error {
            lines.push(Line::from(vec![
                Span::styled("Repo error: ", Style::default().fg(Color::Red)),
                Span::raw(short_message(error)),
            ]));
        }
    }

    if !row.errors.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "Errors",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        for error in &row.errors {
            lines.push(Line::from(format!(
                "{}: {}",
                error.kind,
                short_message(&error.message)
            )));
        }
    }

    if !row.candidate_targets.is_empty() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "Candidates",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for target in &row.candidate_targets {
            lines.push(Line::from(target.clone()));
        }
    }

    if let Some(shadowed_by) = &row.shadowed_by {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled("Shadowed by: ", label_style()),
            Span::raw(shadowed_by.clone()),
        ]));
    }

    lines
}

fn selected_output<'a>(
    app: &'a App,
    selected: &'a SessionSnapshot,
    row: &'a SessionSnapshot,
) -> Option<&'a str> {
    if app.captured_for.as_deref() == Some(selected.display_id.as_str()) {
        return app.captured_output.as_deref();
    }
    if app.inspected_for.as_deref() == Some(selected.display_id.as_str())
        && let Some(detail) = &app.inspected_detail
        && !detail.recent_output.is_empty()
    {
        return Some(detail.recent_output.as_str());
    }
    row.output
        .as_ref()
        .map(|output| {
            if output.recent.is_empty() {
                output.preview.as_str()
            } else {
                output.recent.as_str()
            }
        })
        .filter(|output| !output.is_empty())
}

fn empty_state_lines(app: &App) -> Vec<Line<'static>> {
    if app.snapshots.is_empty() && app.is_polling() {
        return vec![
            Line::from(Span::styled(
                "Polling hosts",
                Style::default().fg(Color::Cyan),
            )),
            Line::from(app.progress_summary()),
        ];
    }

    if !app.filter.as_str().trim().is_empty() {
        return vec![
            Line::from(format!(
                "No rows match filter `{}`",
                app.filter.as_str().trim()
            )),
            Line::from(host_problem_summary(app)),
        ];
    }

    let mut lines = vec![Line::from("No live tmux panes discovered")];
    let problems = unreachable_host_messages(app);
    if !problems.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Unreachable hosts",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
        for problem in problems {
            lines.push(Line::from(problem));
        }
    } else if !app.snapshots.is_empty() {
        lines.push(Line::from("All reachable hosts returned zero panes"));
    }
    lines
}

fn host_problem_summary(app: &App) -> String {
    let problems = unreachable_host_messages(app);
    if problems.is_empty() {
        "No hidden host errors".to_string()
    } else {
        format!("Host problems: {}", problems.join(" | "))
    }
}

fn unreachable_host_messages(app: &App) -> Vec<String> {
    let mut messages = Vec::new();
    for snapshot in &app.snapshots {
        if snapshot.status == SnapshotStatus::Unreachable {
            let message = snapshot
                .errors
                .first()
                .map(|error| short_message(&error.message))
                .unwrap_or_else(|| "unreachable".to_string());
            messages.push(format!("{}: {message}", snapshot.host));
        }
    }
    for progress in &app.host_progress {
        if progress.state == HostProgressState::Unreachable
            && !messages
                .iter()
                .any(|message| message.starts_with(&format!("{}:", progress.id)))
        {
            let message = progress
                .message
                .as_deref()
                .map(short_message)
                .unwrap_or_else(|| "unreachable".to_string());
            messages.push(format!("{}: {message}", progress.id));
        }
    }
    messages
}

fn tail_lines(output: &str, max_lines: usize) -> Vec<String> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| (*line).to_string())
        .collect()
}

fn tmux_target(row: &SessionSnapshot) -> String {
    row.tmux
        .window
        .as_ref()
        .zip(row.tmux.pane.as_ref())
        .map(|(window, pane)| format!("{}:{}.{}", row.tmux.session, window, pane))
        .unwrap_or_else(|| row.tmux.session.clone())
}

fn attach_hint(row: &SessionSnapshot) -> String {
    if let Some(reason) = attach_refusal_reason(row) {
        return format!("unavailable, {reason}");
    }

    if row.watch_id.is_some() {
        format!("enter readonly | a jump -> remux attach {}", row.display_id)
    } else if let Some(target) = &row.raw_target {
        format!("enter readonly | a jump -> remux attach '{target}'")
    } else {
        "unavailable".to_string()
    }
}

fn capture_hint(row: &SessionSnapshot) -> String {
    match row.match_status {
        MatchStatus::Matched => format!("c -> remux capture {} --lines <n>", row.display_id),
        MatchStatus::Orphan => row
            .raw_target
            .as_ref()
            .map(|target| format!("c -> remux capture '{target}' --lines <n>"))
            .unwrap_or_else(|| "unavailable".to_string()),
        MatchStatus::Missing => "unavailable, missing live pane".to_string(),
        MatchStatus::Ambiguous => "unavailable, ambiguous watch".to_string(),
        MatchStatus::Shadowed => format!(
            "unavailable, shadowed by {}",
            row.shadowed_by.as_deref().unwrap_or("another watch")
        ),
        MatchStatus::Unreachable => "unavailable, host unreachable".to_string(),
        MatchStatus::Unknown => "unavailable".to_string(),
    }
}

fn context_project_lines(row: &SessionSnapshot) -> Vec<Line<'static>> {
    let Some(repo) = &row.repo else {
        return vec![Line::from(Span::styled(
            "no project metadata",
            muted_style(),
        ))];
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("project ", label_style()),
            Span::raw(short_path(&repo.path)),
        ]),
        Line::from(vec![
            Span::styled("branch ", label_style()),
            Span::raw(repo.branch.as_deref().unwrap_or("-").to_string()),
            Span::raw("  "),
            Span::styled("dirty ", label_style()),
            Span::styled(
                repo.dirty_count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                dirty_style(
                    &repo
                        .dirty_count
                        .map(|count| count.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ),
        ]),
    ];

    if !repo.changed_files.is_empty() {
        lines.push(Line::from(Span::styled(
            "changed",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for file in repo.changed_files.iter().take(4) {
            lines.push(Line::from(short_message(file)));
        }
        if repo.changed_files.len() > 4 {
            lines.push(Line::from(Span::styled("...", muted_style())));
        }
    }
    if let Some(error) = &repo.error {
        lines.push(Line::from(vec![
            Span::styled("repo error ", Style::default().fg(Color::Red)),
            Span::raw(short_message(error)),
        ]));
    }
    lines
}

fn row_issue_summary(row: &SessionSnapshot) -> Option<String> {
    match row.match_status {
        MatchStatus::Missing => Some(format!("missing configured target {}", row.target)),
        MatchStatus::Ambiguous => Some(format!(
            "ambiguous watch: {} candidates",
            row.candidate_targets.len()
        )),
        MatchStatus::Shadowed => Some(format!(
            "shadowed by {}",
            row.shadowed_by.as_deref().unwrap_or("another watch")
        )),
        MatchStatus::Unreachable => row
            .errors
            .first()
            .map(|error| short_message(&error.message))
            .or_else(|| Some("host unreachable".to_string())),
        MatchStatus::Unknown => row
            .errors
            .first()
            .map(|error| short_message(&error.message))
            .or_else(|| Some("unknown state".to_string())),
        MatchStatus::Matched | MatchStatus::Orphan => None,
    }
}

fn short_path(path: &str) -> String {
    path.strip_prefix("/home/nixos/")
        .or_else(|| path.strip_prefix("/home/cam/"))
        .unwrap_or(path)
        .to_string()
}

fn filter_label(app: &App) -> String {
    let trimmed = app.filter.as_str().trim();
    if trimmed.is_empty() {
        "-".to_string()
    } else {
        trimmed.to_string()
    }
}

fn label_style() -> Style {
    Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD)
}

fn muted_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn row_search_text(row: &SessionSnapshot) -> String {
    let repo_text = row
        .repo
        .as_ref()
        .map(|repo| {
            format!(
                "{} {}",
                repo.path,
                repo.changed_files
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        })
        .unwrap_or_default();
    let output_text = row
        .output
        .as_ref()
        .map(|output| format!("{} {}", output.preview, output.recent))
        .unwrap_or_default();
    let display_command = row.display_command().unwrap_or_default();
    let agent_hint = row.agent_hint.as_deref().unwrap_or("");
    format!(
        "{} {} {} {} {} {} {} {} {} {} {} {}",
        row.host,
        row.container.as_deref().unwrap_or(""),
        row.tmux_socket.as_deref().unwrap_or(""),
        row.display_id,
        row.match_status.as_str(),
        row.state.as_str(),
        row.process
            .as_ref()
            .map(|process| process.command.as_str())
            .unwrap_or(""),
        display_command,
        agent_hint,
        row.process
            .as_ref()
            .map(|process| process.cwd.as_str())
            .unwrap_or(""),
        repo_text,
        output_text
    )
}

fn table_target(row: &SessionSnapshot) -> String {
    row.raw_target
        .clone()
        .or_else(|| {
            if row.target.trim().is_empty() {
                None
            } else {
                Some(row.target.clone())
            }
        })
        .unwrap_or_else(|| "-".to_string())
}

fn row_identity(row: &SessionSnapshot) -> String {
    row.watch_id
        .as_ref()
        .map(|watch_id| format!("watch:{watch_id}"))
        .or_else(|| {
            row.raw_target
                .as_ref()
                .map(|target| format!("target:{target}"))
        })
        .unwrap_or_else(|| format!("display:{}|{}", row.display_id, row.target))
}

fn compare_sort_field(
    left: &SessionSnapshot,
    right: &SessionSnapshot,
    field: TuiSortField,
    direction: TuiSortDirection,
) -> Ordering {
    let ordering = match field {
        TuiSortField::Attention => attention_score(left)
            .cmp(&attention_score(right))
            .then_with(|| state_rank(left.state).cmp(&state_rank(right.state))),
        TuiSortField::LastOutput => last_output_timestamp(left).cmp(&last_output_timestamp(right)),
        TuiSortField::State => state_rank(left.state).cmp(&state_rank(right.state)),
        TuiSortField::Id => left.display_id.cmp(&right.display_id),
    };
    match direction {
        TuiSortDirection::Asc => ordering,
        TuiSortDirection::Desc => ordering.reverse(),
    }
}

fn stable_row_cmp(left: &SessionSnapshot, right: &SessionSnapshot) -> Ordering {
    left.host
        .cmp(&right.host)
        .then_with(|| left.tmux_socket.cmp(&right.tmux_socket))
        .then_with(|| left.tmux.session.cmp(&right.tmux.session))
        .then_with(|| compare_tmux_index(left.tmux.window.as_deref(), right.tmux.window.as_deref()))
        .then_with(|| left.tmux.window.cmp(&right.tmux.window))
        .then_with(|| compare_tmux_index(left.tmux.pane.as_deref(), right.tmux.pane.as_deref()))
        .then_with(|| left.tmux.pane.cmp(&right.tmux.pane))
        .then_with(|| left.tmux.pane_id.cmp(&right.tmux.pane_id))
        .then_with(|| left.raw_target.cmp(&right.raw_target))
        .then_with(|| left.target.cmp(&right.target))
        .then_with(|| left.watch_index.cmp(&right.watch_index))
        .then_with(|| left.display_id.cmp(&right.display_id))
        .then_with(|| row_identity(left).cmp(&row_identity(right)))
}

fn compare_tmux_index(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => match (left.parse::<u32>(), right.parse::<u32>()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            _ => left.cmp(right),
        },
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn sort_label(field: TuiSortField, direction: TuiSortDirection) -> String {
    let field = match field {
        TuiSortField::Attention => "attention",
        TuiSortField::LastOutput => "last-output",
        TuiSortField::State => "state",
        TuiSortField::Id => "id",
    };
    let direction = match direction {
        TuiSortDirection::Asc => "asc",
        TuiSortDirection::Desc => "desc",
    };
    format!("{field} {direction}")
}

fn attention_score(row: &SessionSnapshot) -> usize {
    let mut score = 0;
    score += match row.match_status {
        MatchStatus::Unreachable => 6,
        MatchStatus::Missing => 5,
        MatchStatus::Ambiguous => 4,
        MatchStatus::Shadowed => 3,
        MatchStatus::Unknown => 2,
        MatchStatus::Orphan => 1,
        MatchStatus::Matched => 0,
    };
    score += row.errors.len();
    score += match row.state {
        SessionState::Unreachable => 4,
        SessionState::Missing => 3,
        SessionState::Unknown => 1,
        SessionState::Idle | SessionState::Quiet | SessionState::Active => 0,
    };
    score
}

fn last_output_timestamp(row: &SessionSnapshot) -> i64 {
    row.output
        .as_ref()
        .and_then(|output| output.last_output_at.map(|timestamp| timestamp.timestamp()))
        .unwrap_or(i64::MIN)
}

fn last_output_age(row: &SessionSnapshot) -> String {
    let Some(last_output_at) = row.output.as_ref().and_then(|output| output.last_output_at) else {
        return "-".to_string();
    };
    let age = Utc::now().signed_duration_since(last_output_at);
    if age.num_seconds() < 60 {
        format!("{}s", age.num_seconds().max(0))
    } else if age.num_minutes() < 60 {
        format!("{}m", age.num_minutes())
    } else if age.num_hours() < 24 {
        format!("{}h", age.num_hours())
    } else {
        format!("{}d", age.num_days())
    }
}

fn state_rank(state: SessionState) -> u8 {
    match state {
        SessionState::Active => 6,
        SessionState::Quiet => 5,
        SessionState::Idle => 4,
        SessionState::Unknown => 3,
        SessionState::Missing => 2,
        SessionState::Unreachable => 1,
    }
}

fn match_style(status: MatchStatus) -> Style {
    match status {
        MatchStatus::Matched => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        MatchStatus::Orphan => Style::default().fg(Color::White),
        MatchStatus::Missing => Style::default().fg(Color::Red),
        MatchStatus::Ambiguous => Style::default().fg(Color::Magenta),
        MatchStatus::Shadowed => Style::default().fg(Color::Blue),
        MatchStatus::Unreachable => Style::default().fg(Color::Red),
        MatchStatus::Unknown => Style::default().fg(Color::Gray),
    }
}

fn state_style(state: SessionState) -> Style {
    match state {
        SessionState::Active => Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
        SessionState::Quiet => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::DIM),
        SessionState::Idle => Style::default().fg(Color::DarkGray),
        SessionState::Missing => Style::default().fg(Color::Yellow),
        SessionState::Unreachable => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        SessionState::Unknown => Style::default().fg(Color::Gray),
    }
}

fn dirty_style(value: &str) -> Style {
    match value {
        "-" | "0" => Style::default().fg(Color::DarkGray),
        _ => Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD),
    }
}

fn format_elapsed(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs < 10.0 {
        format!("{:.1}s", secs)
    } else {
        let secs_u = duration.as_secs();
        if secs_u >= 60 {
            format!("{}m{}s", secs_u / 60, secs_u % 60)
        } else {
            format!("{}s", secs_u)
        }
    }
}

fn short_message(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or(message).trim();
    const LIMIT: usize = 48;
    if first_line.chars().count() <= LIMIT {
        first_line.to_string()
    } else {
        format!("{}...", first_line.chars().take(LIMIT).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        HostConfig, HostKind, LaunchTemplatesConfig, PollConfig, WatchConfig, WatchMatchConfig,
    };
    use crate::git::RepoSnapshot;
    use crate::snapshot::{
        HostSnapshot, MatchStatus, OutputSnapshot, ProcessSnapshot, SessionSnapshot, SessionState,
        SnapshotStatus, TmuxSnapshot,
    };

    #[cfg(unix)]
    #[test]
    fn terminal_mode_round_trip_repairs_raw_tty_state() {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::ptr;

        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: openpty initializes both file descriptors on success; the
        // remaining optional output pointers are intentionally null.
        let status = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        assert_eq!(status, 0, "openpty failed: {}", io::Error::last_os_error());
        // SAFETY: openpty returned two newly owned file descriptors.
        let _master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        // SAFETY: openpty returned two newly owned file descriptors.
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };

        let saved = SavedTerminalMode::capture_fd(slave_fd).unwrap();
        let original = saved.mode;
        let mut corrupted = original;
        // SAFETY: `corrupted` is an initialized termios value.
        unsafe { libc::cfmakeraw(&mut corrupted) };
        set_terminal_mode(slave_fd, &corrupted).unwrap();
        let raw = terminal_mode(slave_fd).unwrap();
        assert_ne!(raw.c_lflag, original.c_lflag);

        saved.restore_fd(slave_fd).unwrap();
        let restored = terminal_mode(slave_fd).unwrap();
        assert_eq!(restored.c_iflag, original.c_iflag);
        assert_eq!(restored.c_oflag, original.c_oflag);
        assert_eq!(restored.c_cflag, original.c_cflag);
        let input_behavior_flags =
            libc::ECHO | libc::ECHONL | libc::ICANON | libc::IEXTEN | libc::ISIG;
        assert_eq!(
            restored.c_lflag & input_behavior_flags,
            original.c_lflag & input_behavior_flags
        );
        assert_eq!(restored.c_cc, original.c_cc);

        drop(slave);
    }

    fn make_row(match_status: MatchStatus, state: SessionState) -> SessionSnapshot {
        SessionSnapshot {
            session_id: "test".into(),
            target: "test".into(),
            display_id: "test".into(),
            raw_target: None,
            host: "local".into(),
            container: None,
            tmux_socket: None,
            match_status,
            watch_id: None,
            watch_index: None,
            candidate_targets: vec![],
            shadowed_by: None,
            state,
            agent_hint: None,
            tmux: TmuxSnapshot {
                session: "test".into(),
                window: None,
                pane: None,
                pane_id: None,
                session_attached: None,
                window_name: None,
                pane_title: None,
                host_short: None,
            },
            process: None,
            repo: None,
            output: None,
            errors: vec![],
        }
    }

    fn row_at(
        host: &str,
        session: &str,
        window: &str,
        pane: &str,
        display_id: &str,
    ) -> SessionSnapshot {
        let mut row = make_row(MatchStatus::Orphan, SessionState::Active);
        let target = format!("{host}/{session}:{window}.{pane}");
        row.session_id = display_id.to_string();
        row.target = target.clone();
        row.display_id = display_id.to_string();
        row.raw_target = Some(target);
        row.host = host.to_string();
        row.tmux.session = session.to_string();
        row.tmux.window = Some(window.to_string());
        row.tmux.pane = Some(pane.to_string());
        row.tmux.pane_id = Some(format!("%{window}{pane}"));
        row
    }

    fn row_without_window(
        host: &str,
        session: &str,
        pane: &str,
        display_id: &str,
    ) -> SessionSnapshot {
        let mut row = row_at(host, session, "0", pane, display_id);
        row.tmux.window = None;
        row
    }

    fn host_snapshot(host: &str, sessions: Vec<SessionSnapshot>) -> HostSnapshot {
        HostSnapshot {
            host: host.to_string(),
            tmux_socket: None,
            status: SnapshotStatus::Ok,
            collected_at: Utc::now(),
            sessions,
            errors: Vec::new(),
        }
    }

    fn app_with_rows(rows: Vec<SessionSnapshot>) -> App {
        let mut app = App::new(String::new(), TuiSortField::Id, TuiSortDirection::Asc);
        app.snapshots = vec![host_snapshot("local", rows)];
        app.restore_selection(None);
        app
    }

    #[test]
    fn canonical_state_mapping() {
        let cases = [
            (MatchStatus::Orphan, SessionState::Active, "ready"),
            (MatchStatus::Orphan, SessionState::Idle, "stale"),
            (MatchStatus::Orphan, SessionState::Quiet, "busy"),
            (MatchStatus::Orphan, SessionState::Missing, "missing"),
            (MatchStatus::Orphan, SessionState::Unknown, "-"),
            (MatchStatus::Matched, SessionState::Active, "ready"),
            (MatchStatus::Matched, SessionState::Idle, "stale"),
            (MatchStatus::Shadowed, SessionState::Active, "drift"),
            (MatchStatus::Ambiguous, SessionState::Active, "ambiguous"),
            (MatchStatus::Unreachable, SessionState::Active, "missing"),
            (MatchStatus::Missing, SessionState::Unknown, "missing"),
            (MatchStatus::Unknown, SessionState::Active, "ready"),
        ];
        for (ms, ss, expected) in cases {
            assert_eq!(
                canonical_state(&make_row(ms, ss)),
                expected,
                "canonical_state({ms:?}, {ss:?})"
            );
        }
    }

    #[test]
    fn stable_row_cmp_breaks_duplicate_display_id_ties() {
        let pane_two = row_at("local", "work", "0", "2", "same");
        let pane_one = row_at("local", "work", "0", "1", "same");
        let mut rows = [pane_two, pane_one];

        rows.sort_by(|left, right| {
            compare_sort_field(
                left,
                right,
                TuiSortField::LastOutput,
                TuiSortDirection::Desc,
            )
            .then_with(|| stable_row_cmp(left, right))
        });

        assert_eq!(rows[0].raw_target.as_deref(), Some("local/work:0.1"));
        assert_eq!(rows[1].raw_target.as_deref(), Some("local/work:0.2"));
    }

    #[test]
    fn app_rows_sort_within_tree_groups() {
        let mut app = App::new(String::new(), TuiSortField::Id, TuiSortDirection::Asc);
        app.snapshots = vec![host_snapshot(
            "local",
            vec![
                row_at("local", "beta", "0", "0", "a-pane"),
                row_at("local", "alpha", "0", "0", "z-pane"),
            ],
        )];

        let targets: Vec<&str> = app
            .rows()
            .iter()
            .filter_map(|row| row.raw_target.as_deref())
            .collect();

        assert_eq!(targets, vec!["local/alpha:0.0", "local/beta:0.0"]);
    }

    #[test]
    fn live_table_rows_include_host_session_window_groups() {
        let row = row_at("local", "work", "0", "1", "pane");
        let rows = vec![&row];
        let table_rows = live_table_rows(&rows);

        assert!(matches!(
                    &table_rows[0],
                    LiveTableRow::Group { label, depth: 0 } if label == "host local"
        ));
        assert!(matches!(
            &table_rows[1],
            LiveTableRow::Group { label, depth: 2 } if label == "session work"
        ));
        assert!(matches!(
            &table_rows[2],
            LiveTableRow::Group { label, depth: 3 } if label == "window 0"
        ));
        assert!(matches!(
            &table_rows[3],
            LiveTableRow::Pane { pane_index: 0, .. }
        ));
    }

    #[test]
    fn live_table_rows_group_by_container_when_present() {
        let mut row = row_at("c21-test-container", "api", "0", "0", "pane");
        row.container = Some("test-container".to_string());
        let rows = vec![&row];
        let table_rows = live_table_rows(&rows);

        assert!(matches!(
            &table_rows[0],
            LiveTableRow::Group { label, depth: 0 } if label == "host c21-test-container"
        ));
        assert!(matches!(
            &table_rows[1],
            LiveTableRow::Group { label, depth: 1 } if label == "container test-container"
        ));
        assert!(matches!(
            &table_rows[2],
            LiveTableRow::Group { label, depth: 2 } if label == "session api"
        ));
    }

    #[test]
    fn home_and_gg_select_first_pane() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "0", "1", "pane-1"),
            row_at("local", "work", "0", "2", "pane-2"),
        ]);
        app.select_last_pane();

        assert!(handle_navigation_key(&mut app, key(KeyCode::Home), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.0")
        );

        app.select_last_pane();
        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('g')), 5));
        assert_eq!(app.pending_key, Some(PendingKey::G));
        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('g')), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.0")
        );
        assert_eq!(app.pending_key, None);
    }

    #[test]
    fn end_and_uppercase_g_select_last_pane() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "0", "1", "pane-1"),
            row_at("local", "work", "0", "2", "pane-2"),
        ]);

        assert!(handle_navigation_key(&mut app, key(KeyCode::End), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.2")
        );

        app.select_first_pane();
        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('G')), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.2")
        );
    }

    #[test]
    fn page_movement_clamps_at_list_boundaries() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "0", "1", "pane-1"),
            row_at("local", "work", "0", "2", "pane-2"),
            row_at("local", "work", "0", "3", "pane-3"),
        ]);

        assert!(handle_navigation_key(&mut app, key(KeyCode::PageUp), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.0")
        );

        app.select_last_pane();
        app.ensure_selection_visible(5);
        assert!(handle_navigation_key(&mut app, key(KeyCode::PageDown), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.3")
        );
    }

    #[test]
    fn half_page_movement_uses_at_least_one_row() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "0", "1", "pane-1"),
            row_at("local", "work", "0", "2", "pane-2"),
        ]);

        assert!(handle_navigation_key(&mut app, ctrl('d'), 1));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.1")
        );
        assert!(handle_navigation_key(&mut app, ctrl('u'), 1));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.0")
        );
    }

    #[test]
    fn upward_page_tie_on_group_row_selects_previous_pane() {
        let mut app = app_with_rows(vec![
            row_without_window("local", "alpha", "0", "pane-0"),
            row_without_window("local", "beta", "0", "pane-1"),
        ]);
        app.select_last_pane();

        assert!(handle_navigation_key(&mut app, key(KeyCode::PageUp), 1));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/alpha:0.0")
        );
    }

    #[test]
    fn h_m_l_skip_group_rows_and_select_visible_panes() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "0", "1", "pane-1"),
            row_at("local", "work", "0", "2", "pane-2"),
        ]);
        app.table_offset = 0;

        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('H')), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.0")
        );

        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('M')), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.0")
        );

        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('L')), 5));
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.1")
        );
    }

    #[test]
    fn pending_g_prefix_clears_after_non_g_key() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "0", "1", "pane-1"),
            row_at("local", "work", "0", "2", "pane-2"),
        ]);

        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('g')), 5));
        assert_eq!(app.pending_key, Some(PendingKey::G));
        assert!(handle_navigation_key(&mut app, key(KeyCode::Down), 5));
        assert_eq!(app.pending_key, None);
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/work:0.1")
        );

        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('g')), 5));
        assert!(!handle_navigation_key(&mut app, key(KeyCode::Char('q')), 5));
        assert_eq!(app.pending_key, None);
    }

    #[test]
    fn visible_table_rows_accounts_for_header_and_border() {
        assert_eq!(visible_table_rows_from_height(24), 19);
        assert_eq!(visible_table_rows_from_height(3), 1);
    }

    #[test]
    fn help_lines_fit_normal_terminal_height() {
        let compact = help_lines(78);
        assert!(compact.len() <= 20);
        assert!(
            compact
                .iter()
                .any(|line| line_text(line.clone()).contains("[q] quit"))
        );
    }

    #[test]
    fn status_hint_has_compact_form_for_narrow_terminals() {
        assert!(status_hint(100).len() < status_hint(140).len());
        assert!(status_hint(100).contains("[?] help"));
    }

    #[test]
    fn selected_row_actions_still_resolve_panes_after_tree_navigation() {
        let mut app = app_with_rows(vec![
            row_at("local", "work", "0", "0", "pane-0"),
            row_at("local", "work", "1", "0", "pane-1"),
        ]);
        app.table_offset = 4;

        assert!(handle_navigation_key(&mut app, key(KeyCode::Char('L')), 4));

        let selected = app.selected_row().expect("selected pane row");
        assert_eq!(selected.raw_target.as_deref(), Some("local/work:1.0"));
        assert!(attach_refusal_reason(selected).is_none());
        assert!(PaneTarget::parse(selected.raw_target.as_deref().unwrap()).is_ok());
    }

    #[test]
    fn pending_new_session_selection_falls_back_to_first_live_pane() {
        let mut app = App::new(
            String::new(),
            TuiSortField::Attention,
            TuiSortDirection::Desc,
        );
        app.pending_selection = Some(SelectionPreference::new_session("local", "new-work"));
        app.snapshots = vec![host_snapshot(
            "local",
            vec![
                row_at("local", "old-work", "0", "0", "old"),
                row_at("local", "new-work", "2", "1", "new"),
            ],
        )];

        assert!(app.restore_pending_selection());
        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/new-work:2.1")
        );
        assert!(app.pending_selection.is_none());
    }

    #[test]
    fn pending_new_session_selection_survives_stale_refresh_completion() {
        let mut app = App::new(
            String::new(),
            TuiSortField::Attention,
            TuiSortDirection::Desc,
        );
        app.pending_selection = Some(SelectionPreference::new_session("local", "new-work"));
        app.snapshots = vec![host_snapshot(
            "local",
            vec![row_at("local", "old-work", "0", "0", "old")],
        )];

        let refresh_id = app.begin_refresh(vec!["local".to_string()]);
        app.apply_refresh(RefreshMessage::Complete { refresh_id });

        assert!(app.pending_selection.is_some());

        app.apply_refresh(RefreshMessage::HostFinished {
            refresh_id,
            snapshot: host_snapshot(
                "local",
                vec![
                    row_at("local", "old-work", "0", "0", "old"),
                    row_at("local", "new-work", "0", "0", "new"),
                ],
            ),
        });

        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/new-work:0.0")
        );
        assert!(app.pending_selection.is_none());
    }

    #[test]
    fn refresh_complete_marks_in_flight_hosts_unreachable() {
        let mut app = App::new(
            String::new(),
            TuiSortField::Attention,
            TuiSortDirection::Desc,
        );
        let refresh_id = app.begin_refresh(vec!["local".to_string(), "pi".to_string()]);

        app.apply_refresh(RefreshMessage::HostStarted {
            refresh_id,
            host: "local".to_string(),
        });
        app.apply_refresh(RefreshMessage::Complete { refresh_id });

        assert!(!app.is_polling());
        assert!(app.refresh_completed_at.is_some());
        assert!(app.host_progress.iter().all(|host| {
            matches!(host.state, HostProgressState::Unreachable) && host.finished_at.is_some()
        }));
    }

    #[test]
    fn refresh_deadline_allows_configured_repo_collect_commands() {
        let config = Config {
            poll: PollConfig {
                command_timeout: Duration::from_secs(2),
                ..PollConfig::default()
            },
            tui: Default::default(),
            launch_templates: LaunchTemplatesConfig::default(),
            hosts: vec![HostConfig {
                id: "local".to_string(),
                kind: HostKind::Local,
                tmux_socket: None,
                session_roots: Vec::new(),
                ssh: None,
                container: None,
                container_filter: Vec::new(),
            }],
            watches: vec![
                WatchConfig {
                    id: "one".to_string(),
                    host: "local".to_string(),
                    matcher: WatchMatchConfig {
                        command: Some("node".to_string()),
                        cwd: None,
                        cwd_prefix: None,
                        tmux: None,
                    },
                    repo: Some("/repo/one".to_string()),
                    agent_hint: None,
                },
                WatchConfig {
                    id: "two".to_string(),
                    host: "local".to_string(),
                    matcher: WatchMatchConfig {
                        command: Some("bash".to_string()),
                        cwd: None,
                        cwd_prefix: None,
                        tmux: None,
                    },
                    repo: Some("/repo/two".to_string()),
                    agent_hint: None,
                },
            ],
            sessions: Vec::new(),
        };

        assert_eq!(
            refresh_deadline(&config, &["local".to_string()]),
            Duration::from_secs(12)
        );
    }

    #[test]
    fn stale_refresh_messages_are_ignored_after_new_refresh_starts() {
        let mut app = App::new(
            String::new(),
            TuiSortField::Attention,
            TuiSortDirection::Desc,
        );
        let stale_refresh_id = app.begin_refresh(vec!["local".to_string()]);
        let current_refresh_id = app.begin_refresh(vec!["local".to_string()]);

        app.apply_refresh(RefreshMessage::HostFinished {
            refresh_id: stale_refresh_id,
            snapshot: host_snapshot("local", vec![row_at("local", "old", "0", "0", "old")]),
        });

        assert!(app.snapshots.is_empty());

        app.apply_refresh(RefreshMessage::HostFinished {
            refresh_id: current_refresh_id,
            snapshot: host_snapshot("local", vec![row_at("local", "new", "0", "0", "new")]),
        });

        assert_eq!(
            app.selected_row().and_then(|row| row.raw_target.as_deref()),
            Some("local/new:0.0")
        );
    }

    #[test]
    fn context_rail_preserves_action_hint_at_normal_terminal_height() {
        let mut row = row_at("local", "work", "0", "0", "pane");
        row.process = Some(ProcessSnapshot {
            pid: Some(42),
            command: "node".to_string(),
            cwd: "/repo".to_string(),
        });
        row.repo = Some(RepoSnapshot {
            path: "/repo".to_string(),
            branch: Some("main".to_string()),
            dirty_count: Some(4),
            changed_files: vec![
                " M src/one.rs".to_string(),
                " M src/two.rs".to_string(),
                " M src/three.rs".to_string(),
                " M src/four.rs".to_string(),
            ],
            error: None,
        });
        row.output = Some(OutputSnapshot {
            preview: "line 12".to_string(),
            recent: (1..=12)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            hash: "abc".to_string(),
            last_output_at: Some(Utc::now()),
        });
        let app = app_with_rows(vec![row]);
        let selected = app.selected_row().expect("selected pane");

        let lines = context_rail_lines(&app, selected, 22);
        let rendered: Vec<String> = lines.into_iter().map(line_text).collect();

        assert!(rendered.len() <= 22);
        assert!(
            rendered
                .last()
                .is_some_and(|line| line.contains("[i] refresh")),
            "expected action hint to remain visible: {rendered:?}"
        );
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn line_text(line: Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn line_editor_inserts_at_cursor_and_moves_by_character() {
        let mut editor = LineEditor::new("ac");

        assert!(editor.apply_key(key(KeyCode::Left)));
        assert!(editor.apply_key(key(KeyCode::Char('b'))));

        assert_eq!(editor.as_str(), "abc");
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "ab_c");
    }

    #[test]
    fn line_editor_deletes_before_and_at_cursor() {
        let mut editor = LineEditor::new("abcd");

        editor.apply_key(key(KeyCode::Left));
        editor.apply_key(key(KeyCode::Left));
        editor.apply_key(key(KeyCode::Backspace));
        assert_eq!(editor.as_str(), "acd");
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "a_cd");

        editor.apply_key(key(KeyCode::Delete));
        assert_eq!(editor.as_str(), "ad");
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "a_d");
    }

    #[test]
    fn line_editor_supports_home_end_and_ctrl_movement() {
        let mut editor = LineEditor::new("abcd");

        editor.apply_key(key(KeyCode::Home));
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "_abcd");
        editor.apply_key(ctrl('f'));
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "a_bcd");
        editor.apply_key(ctrl('e'));
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "abcd_");
        editor.apply_key(ctrl('b'));
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "abc_d");
        editor.apply_key(ctrl('a'));
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "_abcd");
    }

    #[test]
    fn line_editor_supports_readline_kill_keys() {
        let mut editor = LineEditor::new("alpha beta  ");

        editor.apply_key(ctrl('w'));
        assert_eq!(editor.as_str(), "alpha ");
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "alpha _");

        editor.apply_key(ctrl('u'));
        assert_eq!(editor.as_str(), "");
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "_");

        for ch in "gamma".chars() {
            editor.apply_key(key(KeyCode::Char(ch)));
        }
        editor.apply_key(key(KeyCode::End));
        editor.apply_key(key(KeyCode::Left));
        editor.apply_key(ctrl('k'));
        assert_eq!(editor.as_str(), "gamm");
        assert_eq!(line_text(Line::from(editor.cursor_spans())), "gamm_");
    }

    #[test]
    fn prompt_rendering_places_cursor_at_editor_cursor() {
        let mut value = LineEditor::new("local/work");
        for _ in 0..4 {
            value.apply_key(key(KeyCode::Left));
        }
        let prompt = InputPrompt {
            kind: InputPromptKind::NewSession,
            value,
        };

        assert!(line_text(input_prompt_line(&prompt)).contains("local/_work"));
    }

    fn row_with_meta(
        cmd: &str,
        window_idx: Option<&str>,
        window_name: Option<&str>,
        pane_title: Option<&str>,
        host_short: Option<&str>,
    ) -> SessionSnapshot {
        let mut row = make_row(MatchStatus::Orphan, SessionState::Active);
        row.process = Some(ProcessSnapshot {
            pid: None,
            command: cmd.to_string(),
            cwd: "/".to_string(),
        });
        row.tmux.window = window_idx.map(str::to_string);
        row.tmux.window_name = window_name.map(str::to_string);
        row.tmux.pane_title = pane_title.map(str::to_string);
        row.tmux.host_short = host_short.map(str::to_string);
        row
    }

    #[test]
    fn friendly_label_drops_default_window_and_title() {
        // window_name == cmd, pane_title == FQDN starting with host_short.
        let row = row_with_meta(
            "kiro-cli",
            Some("0"),
            Some("kiro-cli"),
            Some("dev-dsk-cam.us-west-2.amazon.com"),
            Some("dev-dsk-cam"),
        );
        assert_eq!(friendly_label_suffix(&row), None);
    }

    #[test]
    fn friendly_label_keeps_meaningful_window_name() {
        let row = row_with_meta(
            "kiro-cli",
            Some("3"),
            Some("lrcp"),
            Some("dev-dsk-cam.us-west-2.amazon.com"),
            Some("dev-dsk-cam"),
        );
        assert_eq!(friendly_label_suffix(&row).as_deref(), Some("[lrcp]"));
    }

    #[test]
    fn friendly_label_keeps_meaningful_pane_title() {
        let row = row_with_meta(
            "claude",
            Some("5"),
            Some("ticket"),
            Some("✳ Claude Code"),
            Some("dev-dsk-cam"),
        );
        assert_eq!(
            friendly_label_suffix(&row).as_deref(),
            Some("[ticket | ✳ Claude Code]")
        );
    }

    #[test]
    fn friendly_label_drops_user_at_host_prompt_title() {
        // tmux often sets pane_title to a `user@host:cwd` shell prompt.
        let row = row_with_meta(
            "zsh",
            Some("0"),
            Some("zsh"),
            Some("benchmark@dev-dsk-cam.us-west-2.amazon.com:~/scripts"),
            Some("dev-dsk-cam"),
        );
        assert_eq!(friendly_label_suffix(&row), None);
    }

    #[test]
    fn friendly_label_drops_window_name_matching_window_index() {
        let row = row_with_meta("zsh", Some("2"), Some("2"), None, Some("dev-dsk-cam"));
        assert_eq!(friendly_label_suffix(&row), None);
    }

    #[test]
    fn search_and_command_display_include_detected_pi_agent() {
        let mut row = row_with_meta(
            "node",
            Some("2"),
            None,
            Some("\u{03c0} - work - Read the TaskPacket"),
            Some("local"),
        );
        row.agent_hint = Some("pi".to_string());

        assert_eq!(row.display_command().as_deref(), Some("pi"));
        assert!(row_search_text(&row).contains("pi"));
    }

    #[test]
    fn command_display_keeps_specific_agent_commands() {
        let row = row_with_meta(
            "kiro-cli",
            Some("0"),
            None,
            Some("pi - stale title"),
            Some("local"),
        );

        assert_eq!(row.display_command().as_deref(), Some("kiro-cli"));
        assert!(!row_search_text(&row).contains("pi"));
    }

    #[test]
    fn cwd_prompt_keeps_pending_session_context() {
        let mut app = App::new(
            String::new(),
            TuiSortField::Attention,
            TuiSortDirection::Desc,
        );

        begin_cwd_prompt(&mut app, "local", "work-api", "fzf unavailable");

        let prompt = app.input_prompt.as_ref().unwrap();
        match &prompt.kind {
            InputPromptKind::NewSessionCwd { host, session } => {
                assert_eq!(host, "local");
                assert_eq!(session, "work-api");
            }
            _ => panic!("expected cwd prompt"),
        }
        assert!(prompt.value.as_str().is_empty());
        assert!(app.status.contains("type cwd or press Enter for default"));
    }

    #[test]
    fn launch_cwd_prompt_keeps_pending_template_context() {
        let mut app = App::new(
            String::new(),
            TuiSortField::Attention,
            TuiSortDirection::Desc,
        );
        let preset = LaunchTemplatePreset {
            id: "agent".to_string(),
            label: "Agent".to_string(),
            session_prefix: "agent".to_string(),
            command: "pi".to_string(),
            window_name: Some("agent".to_string()),
        };

        begin_launch_cwd_prompt(
            &mut app,
            "local",
            preset,
            "implement-auth".to_string(),
            "fzf unavailable",
        );

        let prompt = app.input_prompt.as_ref().unwrap();
        match &prompt.kind {
            InputPromptKind::LaunchTemplateCwd {
                host,
                preset,
                suffix,
            } => {
                assert_eq!(host, "local");
                assert_eq!(preset.id, "agent");
                assert_eq!(suffix, "implement-auth");
            }
            _ => panic!("expected launch cwd prompt"),
        }
        assert!(line_text(input_prompt_line(prompt)).contains("local/agent-implement-auth"));
        assert!(prompt.value.as_str().is_empty());
        assert!(app.status.contains("type cwd or press Enter for default"));
    }
}
