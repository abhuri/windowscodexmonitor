use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use image::{Rgba, RgbaImage};
use serde_json::{Value, json};
use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};
use windowscodexmonitor::{
    GaugeZone, MonitorStatus, SessionSignal, WeeklyQuota, classify_monitor_status, parse_weekly_quota,
};
use winit::{
    event::Event,
    event_loop::{ControlFlow, EventLoop, EventLoopProxy},
};

const ICON_SIZE: u32 = 32;

#[derive(Debug)]
enum UserEvent {
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Usage(Result<WeeklyQuota, String>),
    Status(MonitorStatus),
}

#[allow(deprecated)] // tray-icon currently requires winit's closure-based Windows event loop.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|argument| argument == "--diagnose") {
        return diagnose().map_err(Into::into);
    }

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let tray_menu = Menu::new();
    let refresh_item = MenuItem::new("Refresh now", true, None);
    let quit_item = MenuItem::new("Exit", true, None);
    tray_menu.append(&refresh_item)?;
    tray_menu.append(&quit_item)?;

    install_event_handlers(proxy.clone());
    let mut state = AppState::new();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(state.tooltip())
        .with_icon(render_gauge_icon(state.remaining_percent, state.status)?)
        .build()?;
    let event_proxy = proxy.clone();
    request_usage(proxy);
    start_watchdog(event_proxy.clone());

    event_loop.run(move |event, target| {
        target.set_control_flow(ControlFlow::Wait);
        if let Event::UserEvent(event) = event {
            match event {
                UserEvent::Usage(result) => {
                    state.apply_usage(result);
                    update_tray(&tray, &state);
                }
                UserEvent::Status(status) => {
                    state.status = status;
                    update_tray(&tray, &state);
                }
                UserEvent::Menu(menu_event) if menu_event.id == refresh_item.id() => {
                    request_usage(event_proxy.clone());
                }
                UserEvent::Menu(menu_event) if menu_event.id == quit_item.id() => target.exit(),
                UserEvent::Tray(TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                }) => request_usage(event_proxy.clone()),
                _ => {}
            }
        }
    })?;
    Ok(())
}

fn diagnose() -> Result<(), String> {
    let status = read_watchdog_status();
    let quota = read_codex_quota()?;
    let reset = quota
        .resets_at_unix_seconds
        .map(reset_summary)
        .unwrap_or_else(|| "reset unknown".to_owned());
    println!("Weekly remaining: {}%", quota.remaining_percent);
    println!("{reset}");
    println!("Status: {}", status.label());
    Ok(())
}

fn start_watchdog(proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || loop {
        let _ = proxy.send_event(UserEvent::Status(read_watchdog_status()));
        thread::sleep(Duration::from_secs(3));
    });
}

fn read_watchdog_status() -> MonitorStatus {
    let process_present = codex_process_present();
    let Some(path) = most_recent_session_file() else {
        return classify_monitor_status(process_present, SessionSignal::Activity, u64::MAX);
    };
    let age_seconds = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(u64::MAX);
    let signal = read_last_session_signal(&path).unwrap_or(SessionSignal::Activity);
    classify_monitor_status(process_present, signal, age_seconds)
}

fn codex_process_present() -> bool {
    Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq codex.exe", "/FO", "CSV", "/NH"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).to_ascii_lowercase().contains("codex.exe"))
        .unwrap_or(false)
}

fn most_recent_session_file() -> Option<PathBuf> {
    let root = std::env::var_os("USERPROFILE")?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    find_session_files(&Path::new(&root).join(".codex").join("sessions"), &mut newest);
    newest.map(|(_, path)| path)
}

fn find_session_files(directory: &Path, newest: &mut Option<(SystemTime, PathBuf)>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_session_files(&path, newest);
        } else if path.extension().is_some_and(|extension| extension == "jsonl")
            && let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified())
            && newest.as_ref().is_none_or(|(current, _)| modified > *current)
        {
            *newest = Some((modified, path));
        }
    }
}

fn read_last_session_signal(path: &Path) -> Option<SessionSignal> {
    const TAIL_BYTES: u64 = 65_536;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES))).ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    tail.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event.get("payload")?.get("type")?.as_str().map(str::to_owned))
        .next_back()
        .map(|event_type| match event_type.as_str() {
            "task_complete" => SessionSignal::Completed,
            "waitingOnApproval" | "waitingOnUserInput" => SessionSignal::Waiting,
            _ => SessionSignal::Activity,
        })
}

struct AppState {
    remaining_percent: u8,
    status: MonitorStatus,
    reset_at: Option<i64>,
    last_error: Option<String>,
}

impl AppState {
    fn new() -> Self {
        Self {
            remaining_percent: 0,
            status: MonitorStatus::Idle,
            reset_at: None,
            last_error: Some("Loading weekly usage".to_owned()),
        }
    }

    fn apply_usage(&mut self, result: Result<WeeklyQuota, String>) {
        match result {
            Ok(quota) => {
                self.remaining_percent = quota.remaining_percent;
                self.reset_at = quota.resets_at_unix_seconds;
                self.status = MonitorStatus::Idle;
                self.last_error = None;
            }
            Err(error) => {
                self.status = MonitorStatus::Offline;
                self.last_error = Some(error);
            }
        }
    }

    fn tooltip(&self) -> String {
        let reset = self
            .reset_at
            .map(reset_summary)
            .unwrap_or_else(|| "reset unknown".to_owned());
        format!(
            "Codex: {}% left | {} | {}",
            self.remaining_percent,
            reset,
            self.status.label()
        )
    }
}

fn install_event_handlers(proxy: EventLoopProxy<UserEvent>) {
    TrayIconEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event| {
            let _ = proxy.send_event(UserEvent::Tray(event));
        }
    }));
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));
}

fn request_usage(proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        let _ = proxy.send_event(UserEvent::Usage(read_codex_quota()));
    });
}

fn read_codex_quota() -> Result<WeeklyQuota, String> {
    let mut child = Command::new(find_codex_executable()?)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start Codex: {error}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Codex stdin was unavailable".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Codex stdout was unavailable".to_owned())?;

    write_json_line(
        &mut stdin,
        json!({
            "method": "initialize",
            "id": 0,
            "params": {
                "clientInfo": {
                    "name": "windowscodexmonitor",
                    "title": "Windows Codex Monitor",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        }),
    )?;

    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let result = (|| {
                let line = line.map_err(|error| format!("Could not read Codex response: {error}"))?;
                let message: Value = serde_json::from_str(&line)
                    .map_err(|error| format!("Invalid Codex response: {error}"))?;
                match message.get("id").and_then(Value::as_i64) {
                    Some(0) => {
                        write_json_line(&mut stdin, json!({ "method": "initialized", "params": {} }))?;
                        write_json_line(
                            &mut stdin,
                            json!({ "method": "account/rateLimits/read", "id": 1 }),
                        )?;
                        Ok(None)
                    }
                    Some(1) if message.get("error").is_some() => {
                        Err(format!("Codex rejected usage request: {}", message["error"]))
                    }
                    Some(1) => parse_weekly_quota(&line).map(Some).map_err(|error| error.to_string()),
                    _ => Ok(None),
                }
            })();
            match result {
                Ok(Some(quota)) => {
                    let _ = result_sender.send(Ok(quota));
                    return;
                }
                Err(error) => {
                    let _ = result_sender.send(Err(error));
                    return;
                }
                Ok(None) => {}
            }
        }
        let _ = result_sender.send(Err("Codex closed before returning weekly usage".to_owned()));
    });

    let result = result_receiver
        .recv_timeout(Duration::from_secs(12))
        .map_err(|_| "Timed out waiting for Codex weekly usage".to_owned());
    let _ = child.kill();
    result?
}

fn find_codex_executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODEX_MONITOR_CODEX_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "CODEX_MONITOR_CODEX_PATH does not point to a file: {}",
            path.display()
        ));
    }

    let mut candidates = Vec::new();
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            Path::new(&local_app_data)
                .join("OpenAI")
                .join("CodexCli")
                .join("node_modules")
                .join("@openai")
                .join("codex-win32-x64")
                .join("vendor")
                .join("x86_64-pc-windows-msvc")
                .join("bin")
                .join("codex.exe"),
        );
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let windows_apps = Path::new(&program_files).join("WindowsApps");
        if let Ok(entries) = fs::read_dir(windows_apps) {
            candidates.extend(entries.flatten().filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .filter(|name| name.starts_with("OpenAI.Codex_"))
                    .map(|_| entry.path().join("app").join("resources").join("codex.exe"))
            }));
        }
    }

    candidates
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| {
            let fallback = PathBuf::from("codex.exe");
            fallback.is_file().then_some(fallback)
        })
        .ok_or_else(|| {
            "Could not find native codex.exe. Install Codex or set CODEX_MONITOR_CODEX_PATH.".to_owned()
        })
}

fn write_json_line(writer: &mut impl Write, message: Value) -> Result<(), String> {
    writeln!(writer, "{message}").map_err(|error| format!("Could not write Codex request: {error}"))?;
    writer.flush().map_err(|error| format!("Could not send Codex request: {error}"))
}

fn update_tray(tray: &TrayIcon, state: &AppState) {
    if let Ok(icon) = render_gauge_icon(state.remaining_percent, state.status) {
        let _ = tray.set_icon(Some(icon));
    }
    let _ = tray.set_tooltip(Some(state.tooltip()));
}

fn render_gauge_icon(remaining_percent: u8, status: MonitorStatus) -> Result<Icon, tray_icon::BadIcon> {
    let mut image = RgbaImage::from_pixel(ICON_SIZE, ICON_SIZE, Rgba([0, 0, 0, 0]));
    let center = (15.5_f32, 16.0_f32);
    let radius = 12.5_f32;
    let zone_color = match status {
        MonitorStatus::Offline => [120, 124, 132, 255],
        _ => match GaugeZone::for_remaining(remaining_percent) {
            GaugeZone::Green => [67, 201, 122, 255],
            GaugeZone::Yellow => [246, 201, 63, 255],
            GaugeZone::Orange => [239, 142, 50, 255],
            GaugeZone::Red => [228, 75, 75, 255],
        },
    };

    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let dx = x as f32 - center.0;
            let dy = y as f32 - center.1;
            let distance = (dx * dx + dy * dy).sqrt();
            if (radius - 2.0..=radius).contains(&distance) {
                image.put_pixel(x, y, Rgba([57, 62, 70, 255]));
            }
        }
    }

    let pointer_angle = (-135.0 + f32::from(remaining_percent) * 2.7).to_radians();
    let end = (
        center.0 + pointer_angle.cos() * 10.0,
        center.1 + pointer_angle.sin() * 10.0,
    );
    draw_line(&mut image, center, end, Rgba(zone_color));
    draw_disc(&mut image, center, 2.2, Rgba(zone_color));

    Icon::from_rgba(image.into_raw(), ICON_SIZE, ICON_SIZE)
}

fn draw_line(image: &mut RgbaImage, start: (f32, f32), end: (f32, f32), color: Rgba<u8>) {
    let steps = ((end.0 - start.0).abs().max((end.1 - start.1).abs()) * 2.0) as u32;
    for step in 0..=steps {
        let t = step as f32 / steps.max(1) as f32;
        draw_disc(
            image,
            (start.0 + (end.0 - start.0) * t, start.1 + (end.1 - start.1) * t),
            1.0,
            color,
        );
    }
}

fn draw_disc(image: &mut RgbaImage, center: (f32, f32), radius: f32, color: Rgba<u8>) {
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let dx = x as f32 - center.0;
            let dy = y as f32 - center.1;
            if dx * dx + dy * dy <= radius * radius {
                image.put_pixel(x, y, color);
            }
        }
    }
}

fn reset_summary(resets_at_unix_seconds: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(resets_at_unix_seconds);
    let seconds = (resets_at_unix_seconds - now).max(0);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    format!("reset {days}d {hours}h")
}
