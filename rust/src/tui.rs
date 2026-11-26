//! Keyboard-first operator console over `operator::Operator` (M3). It reads and
//! acts only through that API, so it can offer nothing the API would refuse and
//! it never launches a worker itself.
//!
//! * `resume` blocks for the whole resumed run, so it runs on its own thread and
//!   the UI polls for the outcome. That run lives in this process, so quitting is
//!   refused until it ends (stop it with `s`/`S` first); every other quit only
//!   detaches: no control file is written and no worker is touched.
//! * The terminal is restored by a drop guard on every exit path and by
//!   ratatui's panic hook on panic.
//! * Bare `lmloop` is scoped to the canonical Git checkout of the cwd (`Scope::Repo`): only
//!   runs whose manifest repo is exactly that checkout are listed, and `N` reviews and launches
//!   one pilot task through `Operator::launch_pilot` (same guards as `pilot-run`; the manifest
//!   is read once, shown, and the loaded policy is what runs). Outside a repository nothing is
//!   listed: there is no fallback to the global list. `lmloop tui` stays global and cannot launch.
//! * Under 90 columns the run list and the run detail take turns (Enter / Esc).

use crate::lease::LeaseState;
use crate::operator::{
    Actions, Available, Control, Decision, LogTail, MAX_NOTE_BYTES, Operator, ResumeOutcome,
    RunSummary, RunView, TaskState, clean,
};
use crate::serial::Policy;
use crate::state::Result as StateResult;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const TICK: Duration = Duration::from_millis(250);
const REFRESH: Duration = Duration::from_secs(2);
const LOG_BYTES: usize = 16 * 1024;
const SPLIT_MIN_WIDTH: u16 = 90;
const MAX_FIELD_BYTES: usize = 4096;

/// Which runs the console shows.
#[derive(Clone, Debug)]
pub enum Scope {
    /// Every run in the state root (explicit `lmloop tui`); launching is not offered.
    All,
    /// Bare `lmloop` outside a Git repository: nothing is listed.
    NoRepo,
    /// Canonical work-tree root of the cwd.
    Repo(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Detail,
    Logs,
}

enum Mode {
    Normal,
    Help,
    Confirm {
        control: Control,
        id: String,
        reason: String,
    },
    Note {
        decision: Decision,
        id: String,
        text: String,
    },
    /// Operator-typed manifest path and run id for a pilot launch.
    Launch {
        manifest: String,
        id: String,
        on_id: bool,
    },
    /// The loaded policy, shown exactly as it will run; `y` launches.
    Review {
        root: PathBuf,
        id: String,
        manifest: PathBuf,
        policy: Box<Policy>,
        scroll: u16,
    },
}

type ResumeRx = Receiver<StateResult<ResumeOutcome>>;

pub struct App {
    root: PathBuf,
    scope: Scope,
    explicit_root: bool,
    op: Operator,
    runs: Vec<RunSummary>,
    sel: usize,
    view: Option<RunView>,
    tab: Tab,
    /// Index into the selected run's logs; clamped on use, `MAX` means the newest.
    log_sel: usize,
    log: Option<LogTail>,
    scroll: u16,
    detail_open: bool,
    mode: Mode,
    /// A resume or launch running on its own thread: (run id, verb, outcome channel).
    resuming: Option<(String, &'static str, ResumeRx)>,
    last_refresh: Instant,
    pub status: String,
    pub quit: bool,
}

impl App {
    pub fn new(root: &Path) -> Self {
        Self::scoped(root, Scope::All, false)
    }

    /// `explicit_root`: the state root came from `--state-root`, which a launch requires.
    pub fn scoped(root: &Path, scope: Scope, explicit_root: bool) -> Self {
        Self {
            root: root.to_path_buf(),
            scope,
            explicit_root,
            op: Operator::new(root),
            runs: Vec::new(),
            sel: 0,
            view: None,
            tab: Tab::Detail,
            log_sel: usize::MAX,
            log: None,
            scroll: 0,
            detail_open: false,
            mode: Mode::Normal,
            resuming: None,
            last_refresh: Instant::now(),
            status: "? for keys".into(),
            quit: false,
        }
    }

    pub fn is_resuming(&self) -> bool {
        self.resuming.is_some()
    }

    fn selected_id(&self) -> Option<String> {
        self.runs.get(self.sel).map(|r| r.run_id.clone())
    }

    /// Re-read the run list and the selected run, keeping the selection by id.
    pub fn refresh(&mut self) {
        let keep = self.selected_id();
        let listed = match &self.scope {
            Scope::All => self.op.list_runs(),
            Scope::NoRepo => Ok(Vec::new()),
            Scope::Repo(r) => self.op.list_runs_in(r),
        };
        match listed {
            Ok(runs) => self.runs = runs,
            Err(e) => self.status = format!("list failed: {e}"),
        }
        self.sel = keep
            .and_then(|id| self.runs.iter().position(|r| r.run_id == id))
            .unwrap_or(self.sel)
            .min(self.runs.len().saturating_sub(1));
        self.load_view();
        self.last_refresh = Instant::now();
    }

    fn load_view(&mut self) {
        self.view = None;
        if let Some(id) = self.selected_id() {
            match self.op.view(&id) {
                Ok(v) => self.view = Some(v),
                Err(e) => self.status = format!("view failed: {e}"),
            }
        }
        self.load_log();
    }

    fn log_idx(&self) -> Option<usize> {
        let n = self.view.as_ref()?.logs.len();
        (n > 0).then(|| self.log_sel.min(n - 1))
    }

    fn load_log(&mut self) {
        self.log = None;
        if self.tab != Tab::Logs {
            return;
        }
        let (Some(i), Some(v)) = (self.log_idx(), &self.view) else {
            return;
        };
        match self.op.read_log(&v.summary.run_id, &v.logs[i], LOG_BYTES) {
            Ok(t) => self.log = Some(t),
            Err(e) => self.status = format!("log failed: {e}"),
        }
    }

    /// Pick up a finished resume and refresh on the periodic tick.
    pub fn tick(&mut self) {
        if let Some((id, verb, rx)) = &self.resuming {
            let done = match rx.try_recv() {
                Ok(Ok(o)) => Some(format!("{verb} {id}: {}: {}", o.outcome, o.message)),
                Ok(Err(e)) => Some(format!("{verb} {id} failed: {e}")),
                Err(TryRecvError::Disconnected) => Some(format!("{verb} {id}: worker thread died")),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(msg) = done {
                self.status = msg;
                self.resuming = None;
                self.refresh();
            }
        }
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh();
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.normal_key(k),
            Mode::Help => {}
            Mode::Launch {
                mut manifest,
                mut id,
                on_id,
            } => self.launch_key(k, &mut manifest, &mut id, on_id),
            Mode::Review {
                root,
                id,
                manifest,
                policy,
                scroll,
            } => match k.code {
                // Ctrl-Y, never a bare letter: pasted or typed form text must not launch.
                KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.launch(root, id, *policy)
                }
                KeyCode::Char('j') | KeyCode::Down | KeyCode::PageDown => {
                    self.mode = Mode::Review {
                        root,
                        id,
                        manifest,
                        policy,
                        scroll: scroll.saturating_add(if k.code == KeyCode::PageDown {
                            5
                        } else {
                            1
                        }),
                    }
                }
                KeyCode::Char('k') | KeyCode::Up | KeyCode::PageUp => {
                    self.mode = Mode::Review {
                        root,
                        id,
                        manifest,
                        policy,
                        scroll: scroll.saturating_sub(if k.code == KeyCode::PageUp {
                            5
                        } else {
                            1
                        }),
                    }
                }
                _ => self.status = "launch cancelled; nothing started".into(),
            },
            Mode::Confirm {
                control,
                id,
                reason: _,
            } => {
                if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                    self.control(&id, control);
                } else {
                    self.status = "cancelled".into();
                }
            }
            Mode::Note {
                decision,
                id,
                mut text,
            } => match k.code {
                KeyCode::Esc => self.status = "cancelled".into(),
                KeyCode::Enter => match self.op.decide(&id, decision, &text) {
                    Ok(r) => {
                        self.status = format!("{}: {}", r.action, r.message);
                        self.refresh();
                    }
                    Err(e) => {
                        // Keep the note so a refusal costs nothing to fix.
                        self.status = format!("not recorded: {e}");
                        self.mode = Mode::Note { decision, id, text };
                    }
                },
                code => {
                    match code {
                        KeyCode::Backspace => {
                            text.pop();
                        }
                        KeyCode::Char(c)
                            if !c.is_control()
                                && !k.modifiers.contains(KeyModifiers::CONTROL)
                                && text.len() + c.len_utf8() <= MAX_NOTE_BYTES =>
                        {
                            text.push(c)
                        }
                        _ => {}
                    }
                    self.mode = Mode::Note { decision, id, text };
                }
            },
        }
    }

    /// Bracketed paste only fills the launch form (control characters dropped, so a pasted
    /// newline is not Enter); in every other mode it is ignored and can confirm nothing.
    pub fn handle_paste(&mut self, text: &str) {
        if let Mode::Launch {
            manifest,
            id,
            on_id,
        } = &mut self.mode
        {
            let field = if *on_id { id } else { manifest };
            for c in text.chars().filter(|c| !c.is_control()) {
                if field.len() + c.len_utf8() <= MAX_FIELD_BYTES {
                    field.push(c);
                }
            }
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('c') if ctrl => self.try_quit(),
            KeyCode::Char('q') => self.try_quit(),
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Char('r') => {
                self.refresh();
                self.status = "refreshed".into();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_run(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_run(-1),
            KeyCode::Enter => self.detail_open = true,
            KeyCode::Esc => self.detail_open = false,
            KeyCode::Tab => self.set_tab(if self.tab == Tab::Detail {
                Tab::Logs
            } else {
                Tab::Detail
            }),
            KeyCode::Char('1') => self.set_tab(Tab::Detail),
            KeyCode::Char('2') => self.set_tab(Tab::Logs),
            KeyCode::Char('[') => self.move_log(-1),
            KeyCode::Char(']') => self.move_log(1),
            KeyCode::PageDown | KeyCode::Char('d') => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageUp | KeyCode::Char('u') => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::Char('p') => {
                if let Some(id) = self.ready(|a| &a.pause) {
                    self.control(&id, Control::Pause);
                }
            }
            KeyCode::Char('s') => self.confirm(Control::Stop, |a| &a.stop),
            KeyCode::Char('S') => self.confirm(Control::StopNow, |a| &a.stop_now),
            KeyCode::Char('R') => self.resume(),
            KeyCode::Char('N') => self.begin_launch(),
            KeyCode::Char('a') => self.note(Decision::Approved, |a| &a.approve),
            KeyCode::Char('c') => self.note(Decision::ChangesRequested, |a| &a.request_changes),
            _ => {}
        }
    }

    fn try_quit(&mut self) {
        if let Some((id, verb, _)) = &self.resuming {
            self.status = format!(
                "{verb} of {id} runs in this process; stop it (s / S) and wait before quitting"
            );
        } else {
            self.quit = true;
        }
    }

    fn move_run(&mut self, d: isize) {
        let next = self
            .sel
            .saturating_add_signed(d)
            .min(self.runs.len().saturating_sub(1));
        if next != self.sel {
            self.sel = next;
            self.log_sel = usize::MAX;
            self.scroll = 0;
            self.load_view();
        }
    }

    fn move_log(&mut self, d: isize) {
        if let Some(i) = self.log_idx() {
            let n = self.view.as_ref().map_or(1, |v| v.logs.len());
            self.log_sel = i.saturating_add_signed(d).min(n - 1);
            self.set_tab(Tab::Logs);
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.scroll = 0;
        self.load_log();
    }

    /// The selected run's id if the action is on offer; otherwise say why not.
    fn ready(&mut self, pick: fn(&Actions) -> &Available) -> Option<String> {
        let Some(v) = &self.view else {
            self.status = "no run selected".into();
            return None;
        };
        let a = pick(&v.actions);
        if a.available {
            Some(v.summary.run_id.clone())
        } else {
            self.status = format!("not available: {}", a.reason);
            None
        }
    }

    fn confirm(&mut self, control: Control, pick: fn(&Actions) -> &Available) {
        if let Some(id) = self.ready(pick) {
            let reason = self
                .view
                .as_ref()
                .map_or_else(String::new, |v| pick(&v.actions).reason.clone());
            self.mode = Mode::Confirm {
                control,
                id,
                reason,
            };
        }
    }

    fn note(&mut self, decision: Decision, pick: fn(&Actions) -> &Available) {
        if let Some(id) = self.ready(pick) {
            self.mode = Mode::Note {
                decision,
                id,
                text: String::new(),
            };
        }
    }

    fn control(&mut self, id: &str, control: Control) {
        self.status = match self.op.control(id, control) {
            Ok(r) => r.message,
            Err(e) => format!("refused: {e}"),
        };
        self.refresh();
    }

    fn resume(&mut self) {
        if self.resuming.is_some() {
            self.status = "a resume is already running".into();
            return;
        }
        let Some(id) = self.ready(|a| &a.resume) else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        let (root, rid) = (self.root.clone(), id.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Operator::new(&root).resume(&rid));
        });
        self.status = format!("resuming {id} ...");
        self.resuming = Some((id, "resume", rx));
    }

    fn begin_launch(&mut self) {
        if !matches!(self.scope, Scope::Repo(_)) {
            self.status = "N needs a Git checkout: run bare `lmloop` inside one".into();
        } else if !self.explicit_root {
            self.status = "N needs an explicit --state-root".into();
        } else if self.resuming.is_some() {
            self.status = "a resume or launch is already running".into();
        } else {
            self.mode = Mode::Launch {
                manifest: String::new(),
                id: String::new(),
                on_id: false,
            };
        }
    }

    fn launch_key(&mut self, k: KeyEvent, manifest: &mut String, id: &mut String, on_id: bool) {
        let back = |manifest: &mut String, id: &mut String, on_id| Mode::Launch {
            manifest: std::mem::take(manifest),
            id: std::mem::take(id),
            on_id,
        };
        match k.code {
            KeyCode::Esc => self.status = "launch cancelled; nothing started".into(),
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => self.mode = back(manifest, id, !on_id),
            KeyCode::Enter if !on_id => self.mode = back(manifest, id, true),
            KeyCode::Enter => match self.review(manifest, id) {
                Ok(m) => self.mode = m,
                Err(e) => {
                    // Keep the typing so a refusal costs nothing to fix.
                    self.status = format!("not launched: {e}");
                    self.mode = back(manifest, id, on_id);
                }
            },
            code => {
                let field = if on_id { &mut *id } else { &mut *manifest };
                match code {
                    KeyCode::Backspace => {
                        field.pop();
                    }
                    KeyCode::Char(c)
                        if !c.is_control()
                            && !k.modifiers.contains(KeyModifiers::CONTROL)
                            && field.len() + c.len_utf8() <= MAX_FIELD_BYTES =>
                    {
                        field.push(c)
                    }
                    _ => {}
                }
                self.mode = back(manifest, id, on_id);
            }
        }
    }

    /// Load the manifest once through the CLI's pilot guards, then require it to name exactly
    /// this checkout and to live outside it.
    fn review(&self, manifest: &str, id: &str) -> Result<Mode, String> {
        let Scope::Repo(repo) = &self.scope else {
            return Err("no checkout in scope".into());
        };
        let manifest = PathBuf::from(manifest);
        if !manifest.is_absolute() {
            return Err("manifest must be an absolute path".into());
        }
        let outside = std::fs::canonicalize(&manifest)
            .map_err(|e| format!("manifest {}: {e}", manifest.display()))?;
        if outside.starts_with(repo) {
            return Err("manifest must live outside the repository it launches in".into());
        }
        // Read the canonical path just checked, and only if it is a private regular file.
        let (root, id, mut policy) = crate::cli::pilot_prepare(
            Some(self.root.clone()),
            self.explicit_root,
            id,
            &outside,
            true,
        )
        .map_err(|e| e.message)?;
        if std::fs::canonicalize(&policy.repo).ok().as_ref() != Some(repo) {
            return Err(format!(
                "manifest repo {} is not this checkout {}",
                policy.repo.display(),
                repo.display()
            ));
        }
        policy.repo = repo.clone(); // run the canonical checkout that was reviewed
        Ok(Mode::Review {
            root,
            id,
            manifest: outside,
            policy: Box::new(policy),
            scroll: 0,
        })
    }

    /// Run the reviewed policy (never a reread manifest) on its own thread, like a resume.
    fn launch(&mut self, root: PathBuf, id: String, policy: Policy) {
        let (tx, rx) = mpsc::channel();
        let rid = id.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Operator::new(&root).launch_pilot(&rid, policy));
        });
        self.status = format!("launching pilot {id} ...");
        self.resuming = Some((id, "launch", rx));
    }
}

/// Debug-escape control, format and other invisible characters (`\u{202e}`, `\u{200b}`) so
/// the review shows what will really run.
fn visible(s: &str) -> String {
    s.escape_debug().to_string()
}

fn review_lines(root: &Path, id: &str, manifest: &Path, p: &Policy) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let flat = visible;
    let argv = |c: &crate::serial::Cmd| {
        let cap = c
            .timeout_secs
            .map_or("no hard timeout".into(), |t| format!("timeout {t}s"));
        match &c.watchdog {
            Some(w) => format!(
                "{:?} {cap}; watchdog: progress file {} startup {}s idle {}s",
                c.argv,
                visible(&w.progress_file.display().to_string()),
                w.startup_secs,
                w.idle_secs
            ),
            None => format!("{:?} {cap}", c.argv),
        }
    };
    let banner = if p.tasks.len() == 1 {
        "PILOT: one task, one shot, not production verification".to_string()
    } else {
        format!(
            "PILOT: {} serial tasks, one shot, not production verification",
            p.tasks.len()
        )
    };
    let mut out = vec![
        Line::styled(banner, bold),
        Line::from(format!("repo: {}", flat(&p.repo.display().to_string()))),
        Line::from(format!(
            "state root: {}  run id: {id}",
            flat(&root.display().to_string())
        )),
        Line::from(format!(
            "manifest (read once): {}",
            flat(&manifest.display().to_string())
        )),
        Line::from(format!("worker: {}", argv(&p.worker))),
        Line::from(format!("gate: {}", argv(&p.gate))),
    ];
    if let Some(wt) = &p.worktree {
        out.push(Line::from(format!(
            "worktree: {}  branch: {}/{}",
            flat(&wt.root.display().to_string()),
            flat(&wt.branch_prefix),
            flat(id),
        )));
    }
    for t in &p.tasks {
        out.push(Line::styled(
            format!("task {} scopes {:?}", t.id, t.scopes),
            bold,
        ));
        out.push(Line::styled("objective:", bold));
        out.extend(t.prompt.lines().map(|l| Line::from(visible(l))));
    }
    out.push(Line::from(""));
    out.push(Line::styled(
        "Ctrl-Y launches this exact policy | j/k PgUp/PgDn scroll | any other key cancels",
        Style::new().fg(Color::Yellow),
    ));
    out
}

pub fn draw(f: &mut Frame, app: &App) {
    let [head, main, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(f.area());
    let scope = match &app.scope {
        Scope::All => "scope: all runs in this state root (explicit tui)".to_string(),
        Scope::NoRepo => "scope: none -- not inside a Git repository".to_string(),
        Scope::Repo(r) => format!("repo: {}", clean(&r.display().to_string(), false)),
    };
    f.render_widget(
        Paragraph::new(Line::styled(
            scope,
            Style::new().add_modifier(Modifier::BOLD),
        )),
        head,
    );
    if main.width >= SPLIT_MIN_WIDTH {
        let [list, detail] =
            Layout::horizontal([Constraint::Length(36), Constraint::Min(1)]).areas(main);
        draw_list(f, app, list);
        draw_detail(f, app, detail);
    } else if app.detail_open {
        draw_detail(f, app, main);
    } else {
        draw_list(f, app, main);
    }
    draw_footer(f, app, foot);
    match &app.mode {
        Mode::Normal => {}
        Mode::Help => popup(f, " Keys ", help_lines(), 62, 24),
        Mode::Confirm {
            control,
            id,
            reason,
        } => popup(
            f,
            " Confirm ",
            vec![
                Line::from(format!("{} run {id}?", control_name(*control))),
                Line::from(reason.clone()),
                Line::from(""),
                Line::from("y = yes, any other key cancels"),
            ],
            60,
            9,
        ),
        Mode::Launch {
            manifest,
            id,
            on_id,
        } => {
            let mark = |on: bool| if on { "> " } else { "  " };
            popup(
                f,
                " Launch PILOT run (not production) ",
                vec![
                    Line::from("trusted manifest: absolute path, outside this repo, read once"),
                    Line::from(format!("{}manifest: {}", mark(!on_id), tail160(manifest))),
                    Line::from(format!("{}run id:   {}", mark(*on_id), tail160(id))),
                    Line::from(format!(
                        "state root: {}",
                        clean(&app.root.display().to_string(), false)
                    )),
                    Line::from("Enter next/review  Tab switch field  Esc cancel"),
                ],
                80,
                11,
            );
        }
        Mode::Review {
            root,
            id,
            manifest,
            policy,
            scroll,
        } => {
            let a = f.area();
            popup_scroll(
                f,
                " Review PILOT launch ",
                review_lines(root, id, manifest, policy),
                a.width.saturating_sub(2),
                a.height.saturating_sub(2),
                *scroll,
            );
        }
        Mode::Note { decision, id, text } => {
            let shown = tail160(text);
            let (title, hint) = match decision {
                Decision::Approved => (" Approve (final) ", "optional note"),
                Decision::ChangesRequested => (" Request changes (final) ", "note required"),
            };
            popup(
                f,
                title,
                vec![
                    Line::from(format!("run {id}: records only, no Git change ({hint})")),
                    Line::from(format!("> {shown}_")),
                    Line::from(format!("{}/{MAX_NOTE_BYTES} bytes", text.len())),
                    Line::from("Enter records, Esc cancels"),
                ],
                64,
                10,
            );
        }
    }
}

/// The last 160 characters, so a long field keeps its cursor end visible.
fn tail160(text: &str) -> String {
    let n = text.chars().count();
    text.chars().skip(n.saturating_sub(160)).collect()
}

fn control_name(c: Control) -> &'static str {
    match c {
        Control::Pause => "Pause",
        Control::Stop => "Stop",
        Control::StopNow => "STOP NOW",
    }
}

fn draw_list(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .runs
        .iter()
        .map(|r| {
            let mut s = format!(
                "{} {} {}/{}",
                r.run_id, r.phase, r.tasks_done, r.tasks_total
            );
            if r.lease
                .as_ref()
                .is_some_and(|l| l.state == LeaseState::Held)
            {
                s += " live";
            }
            if r.owner_lost {
                s += " LOST";
            }
            if !r.pending_controls.is_empty() {
                s += &format!(" [{}]", r.pending_controls.join(","));
            }
            ListItem::new(s)
        })
        .collect();
    let mut state = ListState::default().with_selected((!items.is_empty()).then_some(app.sel));
    let list = List::new(items)
        .block(Block::bordered().title(format!(" Runs ({}) ", app.runs.len())))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let tabs = match app.tab {
        Tab::Detail => "[1 Detail] 2 Logs",
        Tab::Logs => "1 Detail [2 Logs]",
    };
    let block = Block::bordered().title(format!(" {tabs} "));
    let Some(v) = &app.view else {
        let why = app.runs.get(app.sel).map_or(
            match app.scope {
                Scope::All => "no runs in this state root".to_string(),
                Scope::NoRepo => "not inside a Git repository: no runs are shown".to_string(),
                Scope::Repo(_) => "no runs for this checkout (N launches a pilot)".to_string(),
            },
            |r| r.problem.clone().unwrap_or_else(|| "no view".into()),
        );
        f.render_widget(
            Paragraph::new(why).wrap(Wrap { trim: false }).block(block),
            area,
        );
        return;
    };
    if app.tab == Tab::Logs {
        let inner = block.inner(area);
        let mut lines = vec![match (&app.log, app.log_idx()) {
            (Some(t), Some(i)) => Line::styled(
                format!(
                    "log {}/{} {} ({} bytes{}) [ ] switch",
                    i + 1,
                    v.logs.len(),
                    t.name,
                    t.size,
                    if t.truncated { ", tail only" } else { "" }
                ),
                Style::new().add_modifier(Modifier::BOLD),
            ),
            _ => Line::from("no logs for this run"),
        }];
        if let Some(t) = &app.log {
            let body: Vec<&str> = t.text.lines().collect();
            let room = (inner.height as usize).saturating_sub(1);
            let end = body.len().saturating_sub(app.scroll as usize);
            let from = end.saturating_sub(room);
            lines.extend(
                body[from..end]
                    .iter()
                    .map(|l| Line::from(l.replace('\t', "    "))),
            );
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    f.render_widget(
        Paragraph::new(detail_lines(v))
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0))
            .block(block),
        area,
    );
}

fn detail_lines(v: &RunView) -> Vec<Line<'static>> {
    let s = &v.summary;
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut out = vec![
        Line::styled(format!("{} ({:?})", s.run_id, s.kind), bold),
        Line::from(format!(
            "phase {}  tasks {}/{}{}",
            s.phase,
            s.tasks_done,
            s.tasks_total,
            if s.owner_lost { "  OWNER LOST" } else { "" }
        )),
    ];
    if let Some(branch) = &v.branch {
        out.push(Line::from(format!("worktree branch: {branch}")));
    }
    if let Some(l) = &s.lease {
        out.push(Line::from(format!(
            "lease {:?}{}",
            l.state,
            l.pid.map_or(String::new(), |p| format!(" pid {p}"))
        )));
    }
    if !s.pending_controls.is_empty() {
        out.push(Line::from(format!(
            "pending: {}",
            s.pending_controls.join(", ")
        )));
    }
    if let Some(fail) = &s.failure {
        out.push(Line::styled(
            format!("FAILED: {fail}"),
            Style::new().fg(Color::Red),
        ));
    }
    if let Some(p) = &s.problem {
        out.push(Line::styled(
            format!("problem: {p}"),
            Style::new().fg(Color::Red),
        ));
    }
    out.push(Line::from(format!("evidence: {}", v.evidence)));
    if let Some(d) = &v.decision {
        out.push(Line::styled(
            format!("decision: {:?} - {}", d.decision, d.note),
            Style::new().fg(Color::Cyan),
        ));
    }
    out.push(Line::styled("actions", bold));
    let a = &v.actions;
    for (key, name, x) in [
        ("p", "pause", &a.pause),
        ("s", "stop", &a.stop),
        ("S", "stop-now", &a.stop_now),
        ("R", "resume", &a.resume),
        ("a", "approve", &a.approve),
        ("c", "request changes", &a.request_changes),
    ] {
        let style = Style::new().fg(if x.available {
            Color::Green
        } else {
            Color::DarkGray
        });
        out.push(Line::from(vec![
            Span::styled(format!(" {key} {name}: "), style),
            Span::raw(x.reason.clone()),
        ]));
    }
    out.push(Line::styled("tasks", bold));
    for t in &v.tasks {
        let (mark, color) = match t.state {
            TaskState::Committed => ("ok", Color::Green),
            TaskState::Running => ("..", Color::Yellow),
            TaskState::Failed => ("!!", Color::Red),
            TaskState::Pending => ("--", Color::DarkGray),
        };
        let commit = t.commit.as_deref().map_or("", |c| &c[..c.len().min(8)]);
        out.push(Line::styled(
            format!(" {mark} {} {commit}", t.id),
            Style::new().fg(color),
        ));
    }
    if let Some(w) = &v.watchdog {
        let tripped = w.trip.is_some();
        out.push(Line::styled(
            clean(&crate::serial::watchdog_line(w), false),
            Style::new().fg(if tripped {
                Color::Red
            } else if w.warning.is_some() {
                Color::Yellow
            } else {
                Color::Gray
            }),
        ));
    }
    out.push(Line::styled(
        format!("logs: {} (2 to read)", v.logs.len()),
        bold,
    ));
    out.push(Line::styled("events (newest first)", bold));
    for e in v.events.iter().rev() {
        out.push(Line::from(format!(
            " #{} {} {} {}",
            e.seq,
            e.kind,
            e.task.as_deref().unwrap_or(""),
            e.data
        )));
    }
    for w in &v.warnings {
        out.push(Line::styled(
            format!("warning: {w}"),
            Style::new().fg(Color::Yellow),
        ));
    }
    out
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let busy = app
        .resuming
        .as_ref()
        .map_or(String::new(), |(id, verb, _)| format!("[{verb} {id}] "));
    let hint = match app.mode {
        Mode::Normal if matches!(app.scope, Scope::Repo(_)) => {
            "? help  q quit  r refresh  j/k run  Enter/Esc open/back  N launch pilot  p s S R a c"
        }
        Mode::Normal => "? help  q quit  r refresh  j/k run  Enter/Esc open/back  p s S R a c",
        Mode::Launch { .. } => "type  Tab field  Enter next/review  Esc cancel",
        Mode::Review { .. } => "Ctrl-Y launches  j/k scroll  any other key cancels",
        Mode::Help => "any key closes help",
        Mode::Confirm { .. } => "y confirms, any other key cancels",
        Mode::Note { .. } => "type a note  Enter record  Esc cancel",
    };
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("{busy}{}", app.status),
                Style::new().fg(Color::Yellow),
            ),
            Line::styled(hint, Style::new().add_modifier(Modifier::DIM)),
        ]),
        area,
    );
}

fn help_lines() -> Vec<Line<'static>> {
    [
        "j/k, Up/Down   select run",
        "Enter / Esc    open run / back (narrow terminals)",
        "1 2 / Tab      Detail / Logs",
        "[ ]            previous / next log",
        "PgDn/d PgUp/u  scroll",
        "r              refresh (also every 2s)",
        "p              pause at next task boundary",
        "s              stop at next task boundary (confirm)",
        "S              stop now, tears down worker (confirm)",
        "R              resume (runs the engine in this process)",
        "N              launch one pilot task: review the manifest, Ctrl-Y to run (bare lmloop only)",
        "               pilot runs (SerialPilot) offer only S: no pause, stop or resume",
        "a              approve, with optional note (final)",
        "c              request changes, note required (final)",
        "?              this help",
        "q / Ctrl-C     quit; leaves any worker running",
        "",
        "Unavailable actions say why in the Detail tab.",
        "Approval records a decision only: no Git change.",
    ]
    .into_iter()
    .map(Line::from)
    .collect()
}

fn popup(f: &mut Frame, title: &str, lines: Vec<Line<'static>>, w: u16, h: u16) {
    popup_scroll(f, title, lines, w, h, 0);
}

fn popup_scroll(
    f: &mut Frame,
    title: &str,
    lines: Vec<Line<'static>>,
    w: u16,
    h: u16,
    scroll: u16,
) {
    let a = f.area();
    let (w, h) = (w.min(a.width), h.min(a.height));
    let r = Rect::new(a.x + (a.width - w) / 2, a.y + (a.height - h) / 2, w, h);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(Block::bordered().title(title.to_string())),
        r,
    );
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), event::DisableBracketedPaste);
        ratatui::restore();
    }
}

/// Run the console until the operator quits. The terminal is restored on every path.
pub fn run(root: &Path, scope: Scope, explicit_root: bool) -> io::Result<()> {
    let _restore = Restore;
    let mut term = ratatui::try_init()?; // also installs the restoring panic hook
    crossterm::execute!(io::stdout(), event::EnableBracketedPaste)?;
    let mut app = App::scoped(root, scope, explicit_root);
    app.refresh();
    while !app.quit {
        term.draw(|f| draw(f, &app))?;
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(k) => app.handle_key(k),
                Event::Paste(t) => app.handle_paste(&t),
                _ => {}
            }
        }
        app.tick();
    }
    Ok(())
}
