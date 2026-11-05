use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};
use serde::Deserialize;

const DEFAULT_DIR: &str = "/root/running";
const REFRESH: Duration = Duration::from_secs(2);
const RESOURCE_REFRESH: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct ProcInfo {
    pid: u32,
    age_secs: u64,
}

#[derive(Clone, Debug)]
struct Listener {
    port: u16,
    addr: String,
    pid: Option<u32>,
    comm: String,
}

#[derive(Deserialize)]
struct Manifest {
    name: Option<String>,
    cmd: Option<String>,
    port: Option<u16>,
}

#[derive(Clone, Debug)]
struct App {
    name: String,
    dir: PathBuf,
    cmd: Option<String>,
    port: Option<u16>,
    procs: Vec<ProcInfo>,
    ports: Vec<u16>,
}

impl App {
    fn running(&self) -> bool {
        !self.procs.is_empty()
    }
}

struct RawProc {
    pid: u32,
    comm: String,
    cwd: PathBuf,
    age_secs: u64,
}

#[derive(Clone, Copy)]
struct ProcUsage {
    rss_kb: u64,
    cpu_pct: f64,
}

#[derive(Default)]
struct Resources {
    mem_total_kb: u64,
    mem_used_kb: u64,
    cpu_pct: Option<f64>,
    load1: Option<f64>,
    procs: HashMap<u32, ProcUsage>,
    last_sample: Option<Instant>,
    prev_sys: Option<(u64, u64, Instant)>,
    prev_proc: HashMap<u32, (u64, Instant)>,
}

impl Resources {
    fn due(&self) -> bool {
        self.last_sample
            .map_or(true, |at| at.elapsed() >= RESOURCE_REFRESH)
    }

    fn sample(&mut self, apps: &[App]) {
        let now = Instant::now();
        let hz = clk_tck();
        let uptime = read_uptime();

        if let Some((total, idle)) = read_cpu_totals() {
            let pct = match self.prev_sys {
                Some((prev_total, prev_idle, prev_at)) => {
                    let dt = now.saturating_duration_since(prev_at).as_secs_f64();
                    let dtotal = total.saturating_sub(prev_total) as f64;
                    let didle = idle.saturating_sub(prev_idle) as f64;
                    if dt >= 0.5 && dtotal > 0.0 {
                        Some((1.0 - didle / dtotal).clamp(0.0, 1.0) * 100.0)
                    } else {
                        self.cpu_pct
                    }
                }
                None if total > 0 => Some((1.0 - idle as f64 / total as f64) * 100.0),
                None => None,
            };
            self.cpu_pct = pct;
            self.prev_sys = Some((total, idle, now));
        }

        if let Some((total, available)) = read_meminfo() {
            self.mem_total_kb = total;
            self.mem_used_kb = total.saturating_sub(available);
        }
        self.load1 = read_load1();

        let mut alive = HashSet::new();
        let mut procs = HashMap::new();
        for app in apps {
            for proc in &app.procs {
                alive.insert(proc.pid);
                let Some(rss_kb) = read_rss_kb(proc.pid) else {
                    continue;
                };
                let cpu_pct = match read_cpu_ticks(proc.pid) {
                    Some(ticks) => {
                        let pct = match self.prev_proc.get(&proc.pid).copied() {
                            Some((prev_ticks, prev_at)) => {
                                let dt = now.saturating_duration_since(prev_at).as_secs_f64();
                                if dt >= 0.5 {
                                    let delta = ticks.saturating_sub(prev_ticks) as f64 / hz;
                                    (delta / dt * 100.0).max(0.0)
                                } else {
                                    self.procs.get(&proc.pid).map_or(0.0, |u| u.cpu_pct)
                                }
                            }
                            None => {
                                let age = start_age(proc.pid, uptime).unwrap_or(0).max(1) as f64;
                                ticks as f64 / hz / age * 100.0
                            }
                        };
                        self.prev_proc.insert(proc.pid, (ticks, now));
                        pct
                    }
                    None => self.procs.get(&proc.pid).map_or(0.0, |u| u.cpu_pct),
                };
                procs.insert(proc.pid, ProcUsage { rss_kb, cpu_pct });
            }
        }
        self.prev_proc.retain(|pid, _| alive.contains(pid));
        self.procs = procs;
        self.last_sample = Some(now);
    }

    fn app_usage(&self, app: &App) -> Option<(u64, f64)> {
        let mut rss_kb = 0;
        let mut cpu_pct = 0.0;
        let mut seen = false;
        for proc in &app.procs {
            if let Some(usage) = self.procs.get(&proc.pid) {
                rss_kb += usage.rss_kb;
                cpu_pct += usage.cpu_pct;
                seen = true;
            }
        }
        seen.then_some((rss_kb, cpu_pct))
    }

    fn mem_label(&self) -> String {
        if self.mem_total_kb == 0 {
            "-".to_string()
        } else {
            format!(
                "{}/{}",
                fmt_kb(self.mem_used_kb),
                fmt_kb(self.mem_total_kb)
            )
        }
    }

    fn cpu_label(&self) -> String {
        self.cpu_pct
            .map(|pct| format!("{pct:.1}%"))
            .unwrap_or_else(|| "-".to_string())
    }

    fn load_label(&self) -> String {
        self.load1
            .map(|load| format!("{load:.2}"))
            .unwrap_or_else(|| "-".to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Apps,
    Services,
}

#[derive(Clone, Copy)]
enum Action {
    Start,
    Stop,
    Restart,
}

struct Ui {
    apps: Vec<App>,
    services: Vec<Listener>,
    app_state: TableState,
    svc_state: TableState,
    focus: Focus,
    message: String,
    last_refresh: Instant,
    resources: Resources,
}

impl Ui {
    fn new(root: &Path) -> Self {
        let (apps, services) = scan(root);
        let mut ui = Ui {
            apps,
            services,
            app_state: TableState::default(),
            svc_state: TableState::default(),
            focus: Focus::Apps,
            message: format!("watching {}", root.display()),
            last_refresh: Instant::now(),
            resources: Resources::default(),
        };
        ui.clamp();
        ui.sample_resources();
        ui
    }

    fn sample_resources(&mut self) {
        self.resources.sample(&self.apps);
    }

    fn refresh(&mut self, root: &Path) {
        let (apps, services) = scan(root);
        self.apps = apps;
        self.services = services;
        self.clamp();
        self.last_refresh = Instant::now();
    }

    fn clamp(&mut self) {
        self.app_state.select(if self.apps.is_empty() {
            None
        } else {
            Some(self.app_state.selected().unwrap_or(0).min(self.apps.len() - 1))
        });
        self.svc_state.select(if self.services.is_empty() {
            None
        } else {
            Some(self.svc_state.selected().unwrap_or(0).min(self.services.len() - 1))
        });
    }

    fn move_sel(&mut self, delta: isize) {
        match self.focus {
            Focus::Apps => {
                if self.apps.is_empty() {
                    return;
                }
                let len = self.apps.len() as isize;
                let cur = self.app_state.selected().unwrap_or(0) as isize;
                self.app_state.select(Some((cur + delta).rem_euclid(len) as usize));
            }
            Focus::Services => {
                if self.services.is_empty() {
                    return;
                }
                let len = self.services.len() as isize;
                let cur = self.svc_state.selected().unwrap_or(0) as isize;
                self.svc_state.select(Some((cur + delta).rem_euclid(len) as usize));
            }
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Apps => Focus::Services,
            Focus::Services => Focus::Apps,
        };
    }

    fn manage(&mut self, root: &Path, action: Action) {
        if self.focus != Focus::Apps {
            self.message = "press Tab to focus the apps pane".to_string();
            return;
        }
        let Some(app) = self
            .app_state
            .selected()
            .and_then(|i| self.apps.get(i))
            .cloned()
        else {
            self.message = "no app selected".to_string();
            return;
        };
        let result = match action {
            Action::Start => start_app(&app),
            Action::Stop => stop_app(&app),
            Action::Restart => stop_app(&app).and_then(|_| {
                std::thread::sleep(Duration::from_millis(300));
                start_app(&app)
            }),
        };
        self.message = match result {
            Ok(msg) => msg,
            Err(err) => format!("error: {err}"),
        };
        self.refresh(root);
        self.sample_resources();
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(8),
                Constraint::Length(10),
                Constraint::Length(2),
            ])
            .split(area);

        let running = self.apps.iter().filter(|a| a.running()).count();
        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    " APPDASH ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  {} apps · {} running · {} listeners   ",
                    self.apps.len(),
                    running,
                    self.services.len()
                )),
                Span::styled(clock(), Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(vec![
                Span::styled(" RAM ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.resources.mem_label(),
                    Style::default().fg(Color::Magenta),
                ),
                Span::raw("   "),
                Span::styled("CPU ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.resources.cpu_label(),
                    Style::default().fg(Color::Magenta),
                ),
                Span::raw("   "),
                Span::styled("LOAD ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.resources.load_label(),
                    Style::default().fg(Color::Magenta),
                ),
                Span::styled(
                    "   · resources poll 10s",
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
        ]);
        f.render_widget(header, chunks[0]);

        self.draw_apps(f, chunks[1]);
        self.draw_services(f, chunks[2]);

        let status = Paragraph::new(vec![
            Line::from(Span::styled(self.message.clone(), self.message_style())),
            Line::from(Span::styled(
                "↑/↓ move · Tab focus · s start · x stop · r restart · R refresh · q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        f.render_widget(status, chunks[3]);
    }

    fn draw_apps(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = if self.apps.is_empty() {
            vec![Row::new(vec![
                Cell::from("no apps — add a directory here with app.toml"),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])]
        } else {
            self.apps
                .iter()
                .map(|app| {
                    let (state, state_style) = if app.running() {
                        (
                            "running",
                            Style::default()
                                .fg(Color::Green)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        ("stopped", Style::default().fg(Color::DarkGray))
                    };
                    let mut ports = app.ports.clone();
                    ports.sort_unstable();
                    ports.dedup();
                    let port = if !ports.is_empty() {
                        ports
                            .iter()
                            .map(|p| p.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    } else if let Some(expected) = app.port {
                        format!("{expected}?")
                    } else {
                        "-".to_string()
                    };
                    let pids = if app.procs.is_empty() {
                        "-".to_string()
                    } else {
                        let first = app.procs[0].pid;
                        if app.procs.len() == 1 {
                            first.to_string()
                        } else {
                            format!("{first}+{}", app.procs.len() - 1)
                        }
                    };
                    let age = app
                        .procs
                        .iter()
                        .map(|p| p.age_secs)
                        .max()
                        .map(fmt_age)
                        .unwrap_or_else(|| "-".to_string());
                    let (ram, cpu) = match self.resources.app_usage(app) {
                        Some((rss_kb, cpu_pct)) => {
                            (fmt_kb(rss_kb), format!("{cpu_pct:.1}%"))
                        }
                        None => ("-".to_string(), "-".to_string()),
                    };
                    Row::new(vec![
                        Cell::from(app.name.clone()),
                        Cell::from(state).style(state_style),
                        Cell::from(port),
                        Cell::from(pids),
                        Cell::from(age),
                        Cell::from(ram),
                        Cell::from(cpu),
                        Cell::from(app.dir.display().to_string()),
                    ])
                })
                .collect()
        };

        let table = Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(10),
                Constraint::Length(6),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Min(10),
            ],
        )
        .header(
            Row::new(["APP", "STATE", "PORT", "PIDS", "AGE", "RAM", "CPU", "DIR"])
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(self.border_style(Focus::Apps))
                .title(format!(" apps · {}", self.apps.len())),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
        f.render_stateful_widget(table, area, &mut self.app_state);
    }

    fn draw_services(&mut self, f: &mut Frame, area: Rect) {
        let rows: Vec<Row> = if self.services.is_empty() {
            vec![Row::new(vec![
                Cell::from("-"),
                Cell::from("-"),
                Cell::from("no listeners outside the apps folder"),
                Cell::from(""),
            ])]
        } else {
            self.services
                .iter()
                .map(|listener| {
                    Row::new(vec![
                        Cell::from(listener.port.to_string()),
                        Cell::from(
                            listener
                                .pid
                                .map(|p| p.to_string())
                                .unwrap_or_else(|| "-".to_string()),
                        ),
                        Cell::from(listener.comm.clone()),
                        Cell::from(listener.addr.clone()),
                    ])
                })
                .collect()
        };

        let table = Table::new(
            rows,
            [
                Constraint::Length(7),
                Constraint::Length(8),
                Constraint::Length(20),
                Constraint::Min(18),
            ],
        )
        .header(
            Row::new(["PORT", "PID", "PROCESS", "LISTEN"])
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(self.border_style(Focus::Services))
                .title(" system listeners · read-only "),
        )
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("▶ ");
        f.render_stateful_widget(table, area, &mut self.svc_state);
    }

    fn border_style(&self, want: Focus) -> Style {
        if self.focus == want {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    fn message_style(&self) -> Style {
        if self.message.starts_with("error") {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Yellow)
        }
    }
}

fn main() -> io::Result<()> {
    let root = std::env::var("APPDASH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DIR));

    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;
    let result = run(&mut terminal, &root);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, root: &Path) -> io::Result<()> {
    let mut ui = Ui::new(root);
    loop {
        terminal.draw(|f| ui.draw(f))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Down | KeyCode::Char('j') => ui.move_sel(1),
                    KeyCode::Up | KeyCode::Char('k') => ui.move_sel(-1),
                    KeyCode::Tab => ui.toggle_focus(),
                    KeyCode::Char('R') => {
                        ui.refresh(root);
                        ui.sample_resources();
                        ui.message = "refreshed".to_string();
                    }
                    KeyCode::Char('s') => ui.manage(root, Action::Start),
                    KeyCode::Char('x') => ui.manage(root, Action::Stop),
                    KeyCode::Char('r') => ui.manage(root, Action::Restart),
                    _ => {}
                }
            }
        }
        if ui.last_refresh.elapsed() >= REFRESH {
            ui.refresh(root);
        }
        if ui.resources.due() {
            ui.sample_resources();
        }
    }
}

fn scan(root: &Path) -> (Vec<App>, Vec<Listener>) {
    let mut apps = load_apps(root);
    let raw = scan_processes();

    for proc in &raw {
        if let Some(idx) = apps.iter().position(|app| proc.cwd.starts_with(&app.dir)) {
            apps[idx].procs.push(ProcInfo {
                pid: proc.pid,
                age_secs: proc.age_secs,
            });
        }
    }
    for app in &mut apps {
        app.procs.sort_by_key(|p| p.pid);
    }

    let mut pid_app: HashMap<u32, usize> = HashMap::new();
    let mut pid_comm: HashMap<u32, String> = HashMap::new();
    for (idx, app) in apps.iter().enumerate() {
        for proc in &app.procs {
            pid_app.insert(proc.pid, idx);
        }
    }
    for proc in &raw {
        pid_comm.insert(proc.pid, proc.comm.clone());
    }

    let owners = socket_owners();
    let mut services: HashMap<(Option<u32>, u16), Listener> = HashMap::new();
    for (port, addr, inode) in read_listeners() {
        let pid = owners.get(&inode).copied();
        match pid.and_then(|p| pid_app.get(&p).copied()) {
            Some(idx) => apps[idx].ports.push(port),
            None => {
                services.entry((pid, port)).or_insert_with(|| Listener {
                    port,
                    addr,
                    pid,
                    comm: pid
                        .and_then(|p| pid_comm.get(&p).cloned())
                        .unwrap_or_else(|| "-".to_string()),
                });
            }
        }
    }

    for app in &mut apps {
        app.ports.sort_unstable();
        app.ports.dedup();
    }
    let mut services: Vec<Listener> = services.into_values().collect();
    services.sort_by_key(|l| (l.port, l.pid.unwrap_or(0), l.addr.clone()));
    (apps, services)
}

fn load_apps(root: &Path) -> Vec<App> {
    let mut apps = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return apps;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some(os_name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if os_name.starts_with('.') || os_name == "logs" {
            continue;
        }
        let manifest: Option<Manifest> = fs::read_to_string(dir.join("app.toml"))
            .ok()
            .and_then(|raw| toml::from_str(&raw).ok());
        let name = manifest
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or_else(|| os_name.to_string());
        apps.push(App {
            name,
            cmd: manifest.as_ref().and_then(|m| m.cmd.clone()),
            port: manifest.as_ref().and_then(|m| m.port),
            dir,
            procs: Vec::new(),
            ports: Vec::new(),
        });
    }
    apps
}

fn scan_processes() -> Vec<RawProc> {
    let uptime = read_uptime();
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let age_secs = start_age(pid, uptime).unwrap_or(0);
        out.push(RawProc {
            pid,
            comm,
            cwd,
            age_secs,
        });
    }
    out
}

fn start_age(pid: u32, uptime: f64) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
    let started = ticks as f64 / clk_tck();
    Some((uptime - started).max(0.0) as u64)
}

fn clk_tck() -> f64 {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 {
        hz as f64
    } else {
        100.0
    }
}

fn read_uptime() -> f64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|raw| raw.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn read_cpu_totals() -> Option<(u64, u64)> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let mut fields = stat.lines().next()?.strip_prefix("cpu ")?.split_whitespace();
    let mut total = 0;
    let mut idle = 0;
    for idx in 0..8 {
        let Some(field) = fields.next() else { break };
        let value: u64 = field.parse().ok()?;
        total += value;
        if idx == 3 || idx == 4 {
            idle += value;
        }
    }
    (total > 0).then_some((total, idle))
}

fn read_meminfo() -> Option<(u64, u64)> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = None;
    let mut available = None;
    for line in raw.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let kb = value.split_whitespace().next().and_then(|v| v.parse().ok());
        match key {
            "MemTotal" => total = kb,
            "MemAvailable" => available = kb,
            _ => {}
        }
    }
    Some((total?, available?))
}

fn read_load1() -> Option<f64> {
    fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn read_rss_kb(pid: u32) -> Option<u64> {
    let raw = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    raw.lines().find_map(|line| {
        let rest = line.strip_prefix("VmRSS:")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn read_cpu_ticks(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace();
    let user: u64 = fields.nth(11)?.parse().ok()?;
    let system: u64 = fields.next()?.parse().ok()?;
    Some(user + system)
}

fn fmt_kb(kb: u64) -> String {
    let mib = kb as f64 / 1024.0;
    if mib >= 1024.0 {
        format!("{:.1}G", mib / 1024.0)
    } else if mib >= 99.95 {
        format!("{mib:.0}M")
    } else {
        format!("{mib:.1}M")
    }
}

fn read_listeners() -> Vec<(u16, String, u64)> {
    let mut out = Vec::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue;
            }
            let Some((ip_hex, port_hex)) = fields[1].rsplit_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            let Ok(inode) = fields[9].parse::<u64>() else {
                continue;
            };
            let addr = if v6 {
                format!("[v6]:{port}")
            } else {
                fmt_v4(ip_hex, port)
            };
            out.push((port, addr, inode));
        }
    }
    out
}

fn fmt_v4(hex: &str, port: u16) -> String {
    match u32::from_str_radix(hex, 16) {
        Ok(value) => {
            let [a, b, c, d] = value.to_le_bytes();
            format!("{a}.{b}.{c}.{d}:{port}")
        }
        Err(_) => format!("?.:{port}"),
    }
}

fn socket_owners() -> HashMap<u64, u32> {
    let mut owners = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return owners;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            if let Some(rest) = target.strip_prefix("socket:[") {
                if let Some(inode) = rest.strip_suffix(']').and_then(|s| s.parse::<u64>().ok()) {
                    owners.insert(inode, pid);
                }
            }
        }
    }
    owners
}

fn start_app(app: &App) -> Result<String, String> {
    if app.running() {
        return Err(format!(
            "{} is already running (pid {})",
            app.name, app.procs[0].pid
        ));
    }
    let cmd = app
        .cmd
        .as_deref()
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty())
        .ok_or_else(|| {
            format!(
                "{}: add `cmd` to {}/app.toml",
                app.name,
                app.dir.display()
            )
        })?;

    let logs = app.dir.join("logs");
    fs::create_dir_all(&logs).map_err(|err| format!("create {}: {err}", logs.display()))?;
    let log_path = logs.join("app.log");
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|err| format!("open {}: {err}", log_path.display()))?;
    let _ = writeln!(log, "\n=== {} start {} ===", app.name, clock());
    let log_err = log.try_clone().map_err(|err| format!("clone log: {err}"))?;

    let pidfile = app.dir.join(".appdash.pid");
    let script = format!(
        "echo $$ > {}; exec sh -c {}",
        shell_quote(&pidfile.display().to_string()),
        shell_quote(cmd)
    );
    let status = Command::new("setsid")
        .arg("-f")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .current_dir(&app.dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .status()
        .map_err(|err| format!("spawn setsid: {err}"))?;
    if !status.success() {
        return Err(format!("setsid exited with {status}"));
    }
    std::thread::sleep(Duration::from_millis(400));
    Ok(format!("started {} · logs/app.log", app.name))
}

fn stop_app(app: &App) -> Result<String, String> {
    let mut targets: HashSet<u32> = app.procs.iter().map(|p| p.pid).collect();
    let pidfile = app.dir.join(".appdash.pid");
    let pidfile_pid = fs::read_to_string(&pidfile)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok());
    if let Some(pid) = pidfile_pid {
        if proc_alive(pid) {
            targets.insert(pid);
        }
    }

    let protected = self_and_ancestors();
    targets.retain(|pid| *pid > 1 && !protected.contains(pid));
    if targets.is_empty() {
        let _ = fs::remove_file(&pidfile);
        return Ok(format!("{} is already stopped", app.name));
    }

    let count = targets.len();
    if let Some(pid) = pidfile_pid {
        if pid > 1 && targets.contains(&pid) && !protected.contains(&pid) {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
    }
    for pid in &targets {
        unsafe {
            libc::kill(*pid as i32, libc::SIGTERM);
        }
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && targets.iter().any(|pid| proc_alive(*pid)) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let stubborn: Vec<u32> = targets
        .iter()
        .copied()
        .filter(|pid| proc_alive(*pid))
        .collect();
    for pid in &stubborn {
        unsafe {
            libc::kill(*pid as i32, libc::SIGKILL);
        }
    }

    let _ = fs::remove_file(&pidfile);
    Ok(format!(
        "stopped {} ({} process{})",
        app.name,
        count,
        if count == 1 { "" } else { "es" }
    ))
}

fn proc_alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .map(|(_, rest)| !rest.trim_start().starts_with('Z'))
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn self_and_ancestors() -> HashSet<u32> {
    let mut seen = HashSet::new();
    let mut pid = std::process::id();
    loop {
        if pid <= 1 || !seen.insert(pid) {
            break;
        }
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            break;
        };
        let ppid = rest
            .split_whitespace()
            .nth(1)
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(1);
        pid = ppid;
    }
    seen
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!("{:02}:{:02}:{:02} UTC", secs / 3_600, (secs % 3_600) / 60, secs % 60)
}
