use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{Local, TimeZone};
use font8x8::{BASIC_FONTS, UnicodeFonts};
use fontdue::{Font, FontSettings};
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tray_icon::{
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem},
};
use windowscodexmonitor::{
    GaugeZone, MonitorStatus, SessionSignal, WeeklyQuota, classify_monitor_status,
    parse_weekly_quota,
};
use winit::{
    dpi::{LogicalSize, PhysicalPosition},
    event::{ElementState, Event, MouseButton as WinitMouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy, OwnedDisplayHandle},
    window::{Window, WindowAttributes, WindowLevel},
};

const ICON_SIZE: u32 = 32;
const POPUP_WIDTH: u32 = 390;
const POPUP_HEIGHT: u32 = 310;
const REFRESH_BOUNDS: (f64, f64, f64, f64) = (256.0, 241.0, 114.0, 37.0);
const AUTO_START_BOUNDS: (f64, f64, f64, f64) = (16.0, 282.0, 354.0, 28.0);

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
    let mut preview_popup = std::env::args().any(|argument| argument == "--preview");

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let popup_context = softbuffer::Context::new(event_loop.owned_display_handle())?;
    let proxy = event_loop.create_proxy();
    let mut settings = AppSettings::load();
    let tray_menu = Menu::new();
    let refresh_item = MenuItem::new("Refresh now", true, None);
    let auto_start_item = CheckMenuItem::new(
        "Start with Windows",
        true,
        settings.start_with_windows,
        None,
    );
    let quit_item = MenuItem::new("Exit", true, None);
    tray_menu.append(&refresh_item)?;
    tray_menu.append(&auto_start_item)?;
    tray_menu.append(&quit_item)?;

    install_event_handlers(proxy.clone());
    let mut state = AppState::new();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(state.tooltip())
        .with_icon(render_gauge_icon(state.remaining_percent, state.status)?)
        .build()?;
    let event_proxy = proxy.clone();
    let mut popup = None;
    request_usage(proxy);
    start_watchdog(event_proxy.clone());

    event_loop.run(move |event, target| {
        target.set_control_flow(ControlFlow::Wait);
        if preview_popup {
            show_popup(target, &popup_context, &mut popup);
            preview_popup = false;
        }
        if let Event::UserEvent(event) = event {
            match event {
                UserEvent::Usage(result) => {
                    state.apply_usage(result);
                    update_tray(&tray, &state);
                    request_popup_redraw(&popup);
                }
                UserEvent::Status(status) => {
                    state.status = status;
                    update_tray(&tray, &state);
                    request_popup_redraw(&popup);
                }
                UserEvent::Menu(menu_event) if menu_event.id == auto_start_item.id() => {
                    settings.start_with_windows = auto_start_item.is_checked();
                    if let Err(error) = set_auto_start(settings.start_with_windows) {
                        settings.start_with_windows = !settings.start_with_windows;
                        auto_start_item.set_checked(settings.start_with_windows);
                        state.last_error = Some(error);
                    } else if let Err(error) = settings.save() {
                        state.last_error = Some(error);
                    }
                    update_tray(&tray, &state);
                    request_popup_redraw(&popup);
                }
                UserEvent::Menu(menu_event) if menu_event.id == refresh_item.id() => {
                    request_usage(event_proxy.clone());
                }
                UserEvent::Menu(menu_event) if menu_event.id == quit_item.id() => target.exit(),
                UserEvent::Tray(TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                }) => {
                    show_popup(target, &popup_context, &mut popup);
                    request_usage(event_proxy.clone());
                }
                _ => {}
            }
        } else if let Event::WindowEvent { window_id, event } = event
            && popup
                .as_ref()
                .is_some_and(|current| current.window.id() == window_id)
        {
            handle_popup_event(
                target,
                &event_proxy,
                &mut settings,
                &auto_start_item,
                &mut state,
                &mut popup,
                event,
            );
        }
    })?;
    Ok(())
}

struct Popup {
    window: Rc<Window>,
    surface: softbuffer::Surface<OwnedDisplayHandle, Rc<Window>>,
    cursor: PhysicalPosition<f64>,
    font: Option<Font>,
}

fn show_popup(
    target: &ActiveEventLoop,
    context: &softbuffer::Context<OwnedDisplayHandle>,
    popup: &mut Option<Popup>,
) {
    if let Some(existing) = popup {
        existing.window.set_visible(true);
        existing.window.focus_window();
        existing.window.request_redraw();
        return;
    }

    let attributes = WindowAttributes::default()
        .with_title("Windows Codex Monitor")
        .with_inner_size(LogicalSize::new(
            f64::from(POPUP_WIDTH),
            f64::from(POPUP_HEIGHT),
        ))
        .with_min_inner_size(LogicalSize::new(
            f64::from(POPUP_WIDTH),
            f64::from(POPUP_HEIGHT),
        ))
        .with_max_inner_size(LogicalSize::new(
            f64::from(POPUP_WIDTH),
            f64::from(POPUP_HEIGHT),
        ))
        .with_resizable(false)
        .with_window_level(WindowLevel::AlwaysOnTop);
    let Ok(window) = target.create_window(attributes) else {
        return;
    };
    let window = Rc::new(window);
    let Ok(surface) = softbuffer::Surface::new(context, window.clone()) else {
        return;
    };
    window.request_redraw();
    *popup = Some(Popup {
        window,
        surface,
        cursor: PhysicalPosition::new(-1.0, -1.0),
        font: load_system_font(),
    });
}

fn request_popup_redraw(popup: &Option<Popup>) {
    if let Some(popup) = popup {
        popup.window.request_redraw();
    }
}

fn handle_popup_event(
    _target: &ActiveEventLoop,
    proxy: &EventLoopProxy<UserEvent>,
    settings: &mut AppSettings,
    auto_start_item: &CheckMenuItem,
    state: &mut AppState,
    popup: &mut Option<Popup>,
    event: WindowEvent,
) {
    match event {
        WindowEvent::CloseRequested => *popup = None,
        WindowEvent::CursorMoved { position, .. } => {
            if let Some(popup) = popup {
                popup.cursor = position;
            }
        }
        WindowEvent::MouseInput {
            state: ElementState::Released,
            button: WinitMouseButton::Left,
            ..
        } => {
            let Some(current) = popup.as_ref() else {
                return;
            };
            let logical_cursor = current
                .cursor
                .to_logical::<f64>(current.window.scale_factor());
            if contains(logical_cursor, REFRESH_BOUNDS) {
                request_usage(proxy.clone());
            } else if contains(logical_cursor, AUTO_START_BOUNDS) {
                settings.start_with_windows = !settings.start_with_windows;
                if let Err(error) = set_auto_start(settings.start_with_windows) {
                    settings.start_with_windows = !settings.start_with_windows;
                    state.last_error = Some(error);
                } else if let Err(error) = settings.save() {
                    state.last_error = Some(error);
                }
                auto_start_item.set_checked(settings.start_with_windows);
                request_popup_redraw(popup);
            }
        }
        WindowEvent::RedrawRequested => {
            if let Some(popup) = popup.as_mut() {
                let _ = render_popup(popup, state, settings);
            }
        }
        _ => {}
    }
}

fn contains(position: winit::dpi::LogicalPosition<f64>, bounds: (f64, f64, f64, f64)) -> bool {
    position.x >= bounds.0
        && position.x <= bounds.0 + bounds.2
        && position.y >= bounds.1
        && position.y <= bounds.1 + bounds.3
}

fn render_popup(popup: &mut Popup, state: &AppState, settings: &AppSettings) -> Result<(), String> {
    let size = popup.window.inner_size();
    let width = NonZeroU32::new(size.width).ok_or_else(|| "Popup width was zero".to_owned())?;
    let height = NonZeroU32::new(size.height).ok_or_else(|| "Popup height was zero".to_owned())?;
    popup
        .surface
        .resize(width, height)
        .map_err(|error| error.to_string())?;
    let mut buffer = popup
        .surface
        .buffer_mut()
        .map_err(|error| error.to_string())?;
    let (width, height) = (buffer.width().get(), buffer.height().get());
    buffer.fill(rgb(15, 23, 42));
    let scale = width as f32 / POPUP_WIDTH as f32;
    rounded_rect(
        &mut buffer,
        width,
        height,
        16.0,
        14.0,
        358.0,
        294.0,
        16.0,
        rgb(24, 34, 55),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        32.0,
        31.0,
        18.0,
        "Codex Monitor",
        rgb(241, 245, 249),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        32.0,
        55.0,
        12.0,
        "WEEKLY USAGE",
        rgb(148, 163, 184),
        scale,
    );
    draw_popup_gauge(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        state.remaining_percent,
        state.status,
        scale,
    );

    let percentage = format!("{}%", state.remaining_percent);
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        154.0,
        66.0,
        48.0,
        &percentage,
        zone_color(state),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        158.0,
        118.0,
        14.0,
        "weekly remaining",
        rgb(203, 213, 225),
        scale,
    );
    rounded_rect(
        &mut buffer,
        width,
        height,
        158.0,
        143.0,
        164.0,
        28.0,
        14.0,
        status_color(state.status),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        173.0,
        151.0,
        12.0,
        state.status.label(),
        rgb(15, 23, 42),
        scale,
    );

    let reset_remaining = state
        .reset_at
        .map(reset_summary)
        .unwrap_or_else(|| "reset unknown".to_owned());
    let reset_date = state
        .reset_at
        .map(reset_absolute)
        .unwrap_or_else(|| "Date unavailable".to_owned());
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        32.0,
        190.0,
        12.0,
        "RESETS IN",
        rgb(148, 163, 184),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        32.0,
        207.0,
        18.0,
        &reset_remaining,
        rgb(241, 245, 249),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        32.0,
        232.0,
        12.0,
        &reset_date,
        rgb(148, 163, 184),
        scale,
    );

    rounded_rect(
        &mut buffer,
        width,
        height,
        256.0,
        241.0,
        114.0,
        37.0,
        10.0,
        rgb(59, 130, 246),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        281.0,
        253.0,
        13.0,
        "Refresh",
        rgb(255, 255, 255),
        scale,
    );
    rounded_rect(
        &mut buffer,
        width,
        height,
        16.0,
        282.0,
        354.0,
        28.0,
        10.0,
        rgb(30, 41, 59),
        scale,
    );
    draw_ui_text(
        &mut buffer,
        width,
        height,
        popup.font.as_ref(),
        31.0,
        290.0,
        12.0,
        "Start with Windows",
        rgb(203, 213, 225),
        scale,
    );
    let toggle_color = if settings.start_with_windows {
        rgb(67, 201, 122)
    } else {
        rgb(100, 107, 119)
    };
    rounded_rect(
        &mut buffer,
        width,
        height,
        326.0,
        288.0,
        30.0,
        16.0,
        8.0,
        toggle_color,
        scale,
    );
    draw_disc_u32(
        &mut buffer,
        width,
        height,
        (
            if settings.start_with_windows {
                347.0 * scale
            } else {
                335.0 * scale
            },
            296.0 * scale,
        ),
        6.0 * scale,
        rgb(245, 247, 250),
    );
    if let Some(error) = &state.last_error {
        draw_ui_text(
            &mut buffer,
            width,
            height,
            popup.font.as_ref(),
            32.0,
            252.0,
            11.0,
            &truncate(error, 28),
            rgb(238, 138, 79),
            scale,
        );
    }
    buffer.present().map_err(|error| error.to_string())
}

fn zone_color(state: &AppState) -> u32 {
    if state.status == MonitorStatus::Offline {
        rgb(140, 147, 160)
    } else {
        match GaugeZone::for_remaining(state.remaining_percent) {
            GaugeZone::Green => rgb(67, 201, 122),
            GaugeZone::Yellow => rgb(246, 201, 63),
            GaugeZone::Orange => rgb(239, 142, 50),
            GaugeZone::Red => rgb(228, 75, 75),
        }
    }
}

fn status_color(status: MonitorStatus) -> u32 {
    match status {
        MonitorStatus::Working => rgb(67, 201, 122),
        MonitorStatus::Waiting => rgb(246, 201, 63),
        MonitorStatus::Idle => rgb(170, 178, 188),
        MonitorStatus::Offline => rgb(140, 147, 160),
        MonitorStatus::SuspectedHung => rgb(239, 142, 50),
    }
}

fn draw_popup_gauge(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    font: Option<&Font>,
    remaining_percent: u8,
    status: MonitorStatus,
    scale: f32,
) {
    let center = (82.0 * scale, 147.0 * scale);
    let radius = 57.0 * scale;
    let ring_width = 7.0 * scale;
    for y in 0..height {
        for x in 0..width {
            let dx = x as f32 - center.0;
            let dy = y as f32 - center.1;
            let distance = (dx * dx + dy * dy).sqrt();
            let angle = dy.atan2(dx);
            if (radius - ring_width..=radius).contains(&distance)
                && (-std::f32::consts::PI..=0.0).contains(&angle)
            {
                let percent =
                    ((angle + std::f32::consts::PI) / std::f32::consts::PI * 100.0).round() as u8;
                buffer[(y * width + x) as usize] = gauge_band_color(percent);
            }
        }
    }
    for percent in [0_u8, 25, 50, 75, 100] {
        let angle = (-180.0 + f32::from(percent) * 1.8).to_radians();
        let outer = (
            center.0 + angle.cos() * (radius + 2.0 * scale),
            center.1 + angle.sin() * (radius + 2.0 * scale),
        );
        let inner = (
            center.0 + angle.cos() * (radius - 10.0 * scale),
            center.1 + angle.sin() * (radius - 10.0 * scale),
        );
        draw_line_u32(
            buffer,
            width,
            height,
            outer,
            inner,
            1.0 * scale,
            rgb(226, 232, 240),
        );
    }
    let angle = (-180.0 + f32::from(remaining_percent) * 1.8).to_radians();
    let end = (
        center.0 + angle.cos() * 43.0 * scale,
        center.1 + angle.sin() * 43.0 * scale,
    );
    let needle_color = zone_color_for(status, remaining_percent);
    draw_line_u32(
        buffer,
        width,
        height,
        center,
        end,
        2.0 * scale,
        needle_color,
    );
    draw_disc_u32(buffer, width, height, center, 5.0 * scale, needle_color);
    draw_ui_text(
        buffer,
        width,
        height,
        font,
        20.0,
        155.0,
        11.0,
        "0",
        rgb(148, 163, 184),
        scale,
    );
    draw_ui_text(
        buffer,
        width,
        height,
        font,
        118.0,
        155.0,
        11.0,
        "100",
        rgb(148, 163, 184),
        scale,
    );
}

fn gauge_band_color(remaining_percent: u8) -> u32 {
    match GaugeZone::for_remaining(remaining_percent) {
        GaugeZone::Green => rgb(67, 201, 122),
        GaugeZone::Yellow => rgb(246, 201, 63),
        GaugeZone::Orange => rgb(239, 142, 50),
        GaugeZone::Red => rgb(228, 75, 75),
    }
}

fn zone_color_for(status: MonitorStatus, remaining_percent: u8) -> u32 {
    zone_color(&AppState {
        remaining_percent,
        status,
        reset_at: None,
        last_error: None,
    })
}

fn load_system_font() -> Option<Font> {
    let path = Path::new(r"C:\Windows\Fonts\segoeui.ttf");
    let bytes = fs::read(path).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

#[allow(clippy::too_many_arguments)] // Popup software renderer passes its compact drawing context explicitly.
fn draw_ui_text(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    font: Option<&Font>,
    x: f32,
    y: f32,
    point_size: f32,
    text: &str,
    color: u32,
    dpi_scale: f32,
) {
    let Some(font) = font else {
        draw_text(
            buffer,
            width,
            height,
            x,
            y,
            (point_size / 8.0).max(1.0) as u32,
            text,
            color,
            dpi_scale,
        );
        return;
    };
    let mut cursor_x = (x * dpi_scale).round() as i32;
    // Keep every glyph on the font's baseline. The prior renderer placed each
    // bitmap at its own top edge, which made digits, symbols, and letters look
    // vertically misaligned.
    let baseline_y = (y * dpi_scale).round() as i32
        + font
            .horizontal_line_metrics(point_size * dpi_scale)
            .map(|metrics| metrics.ascent.round() as i32)
            .unwrap_or_else(|| (point_size * dpi_scale * 0.8).round() as i32);
    for character in text.chars() {
        let (metrics, bitmap) = font.rasterize(character, point_size * dpi_scale);
        for glyph_y in 0..metrics.height {
            for glyph_x in 0..metrics.width {
                let alpha = bitmap[glyph_y * metrics.width + glyph_x];
                if alpha == 0 {
                    continue;
                }
                let pixel_x = cursor_x + glyph_x as i32 + metrics.xmin;
                let pixel_y = baseline_y - metrics.height as i32 - metrics.ymin + glyph_y as i32;
                if pixel_x >= 0 && pixel_y >= 0 && pixel_x < width as i32 && pixel_y < height as i32
                {
                    let index = (pixel_y as u32 * width + pixel_x as u32) as usize;
                    buffer[index] = blend(buffer[index], color, alpha);
                }
            }
        }
        cursor_x += metrics.advance_width.round() as i32;
    }
}

#[allow(clippy::too_many_arguments)] // A small software-drawn rounded rectangle avoids a GUI framework dependency.
fn rounded_rect(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    rect_width: f32,
    rect_height: f32,
    radius: f32,
    color: u32,
    scale: f32,
) {
    let x = (x * scale).round() as i32;
    let y = (y * scale).round() as i32;
    let rect_width = (rect_width * scale).round() as i32;
    let rect_height = (rect_height * scale).round() as i32;
    let radius = radius * scale;
    for row in y.max(0)..(y + rect_height).min(height as i32) {
        for column in x.max(0)..(x + rect_width).min(width as i32) {
            let nearest_x =
                (column as f32).clamp(x as f32 + radius, (x + rect_width) as f32 - radius);
            let nearest_y =
                (row as f32).clamp(y as f32 + radius, (y + rect_height) as f32 - radius);
            let dx = column as f32 - nearest_x;
            let dy = row as f32 - nearest_y;
            if dx * dx + dy * dy <= radius * radius {
                buffer[(row as u32 * width + column as u32) as usize] = color;
            }
        }
    }
}

fn blend(background: u32, foreground: u32, alpha: u8) -> u32 {
    let alpha = u32::from(alpha);
    let blend_channel = |shift: u32| {
        let background = (background >> shift) & 0xff_u32;
        let foreground = (foreground >> shift) & 0xff_u32;
        (background * (255 - alpha) + foreground * alpha) / 255
    };
    (blend_channel(16) << 16) | (blend_channel(8) << 8) | blend_channel(0)
}

#[allow(clippy::too_many_arguments)] // Coordinates and pixel buffer stay explicit in this small software renderer.
fn draw_text(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    glyph_scale: u32,
    text: &str,
    color: u32,
    dpi_scale: f32,
) {
    let glyph_scale = (glyph_scale as f32 * dpi_scale).round().max(1.0) as u32;
    let mut cursor_x = (x * dpi_scale).round() as i32;
    let cursor_y = (y * dpi_scale).round() as i32;
    for character in text.chars() {
        if let Some(glyph) = BASIC_FONTS.get(character) {
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..8 {
                    if bits & (1 << column) != 0 {
                        fill_rect_pixels(
                            buffer,
                            width,
                            height,
                            cursor_x + column * glyph_scale as i32,
                            cursor_y + row as i32 * glyph_scale as i32,
                            glyph_scale,
                            glyph_scale,
                            color,
                        );
                    }
                }
            }
        }
        cursor_x += 8 * glyph_scale as i32;
    }
}

#[allow(clippy::too_many_arguments)] // Low-level primitive intentionally exposes buffer and rectangle dimensions.
fn fill_rect_pixels(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    rect_width: u32,
    rect_height: u32,
    color: u32,
) {
    for row in y.max(0) as u32..(y + rect_height as i32).max(0) as u32 {
        for column in x.max(0) as u32..(x + rect_width as i32).max(0) as u32 {
            if column < width && row < height {
                buffer[(row * width + column) as usize] = color;
            }
        }
    }
}

fn draw_line_u32(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    start: (f32, f32),
    end: (f32, f32),
    radius: f32,
    color: u32,
) {
    let steps = ((end.0 - start.0).abs().max((end.1 - start.1).abs()) * 2.0) as u32;
    for step in 0..=steps {
        let t = step as f32 / steps.max(1) as f32;
        draw_disc_u32(
            buffer,
            width,
            height,
            (
                start.0 + (end.0 - start.0) * t,
                start.1 + (end.1 - start.1) * t,
            ),
            radius,
            color,
        );
    }
}

fn draw_disc_u32(
    buffer: &mut [u32],
    width: u32,
    height: u32,
    center: (f32, f32),
    radius: f32,
    color: u32,
) {
    let start_y = (center.1 - radius).floor().max(0.0) as u32;
    let end_y = (center.1 + radius).ceil().min(height as f32) as u32;
    let start_x = (center.0 - radius).floor().max(0.0) as u32;
    let end_x = (center.0 + radius).ceil().min(width as f32) as u32;
    for y in start_y..end_y {
        for x in start_x..end_x {
            let dx = x as f32 - center.0;
            let dy = y as f32 - center.1;
            if dx * dx + dy * dy <= radius * radius {
                buffer[(y * width + x) as usize] = color;
            }
        }
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> u32 {
    u32::from(blue) | (u32::from(green) << 8) | (u32::from(red) << 16)
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        format!(
            "{}...",
            text.chars()
                .take(limit.saturating_sub(3))
                .collect::<String>()
        )
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AppSettings {
    #[serde(default)]
    start_with_windows: bool,
}

impl AppSettings {
    fn path() -> Option<PathBuf> {
        Some(
            PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
                .join("WindowsCodexMonitor")
                .join("settings.json"),
        )
    }

    fn load() -> Self {
        Self::path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|contents| serde_json::from_str(&contents).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or_else(|| "LOCALAPPDATA is unavailable".to_owned())?;
        let directory = path
            .parent()
            .ok_or_else(|| "Settings directory is unavailable".to_owned())?;
        fs::create_dir_all(directory)
            .map_err(|error| format!("Could not create settings directory: {error}"))?;
        fs::write(
            path,
            serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("Could not save settings: {error}"))
    }
}

fn set_auto_start(enabled: bool) -> Result<(), String> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (run, _) = hkcu
        .create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map_err(|error| format!("Could not open Windows startup settings: {error}"))?;
    if enabled {
        let executable = std::env::current_exe()
            .map_err(|error| format!("Could not locate monitor executable: {error}"))?;
        run.set_value(
            "WindowsCodexMonitor",
            &format!("\"{}\"", executable.display()),
        )
        .map_err(|error| format!("Could not enable auto-start: {error}"))
    } else {
        match run.delete_value("WindowsCodexMonitor") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Could not disable auto-start: {error}")),
        }
    }
}

fn diagnose() -> Result<(), String> {
    let status = read_watchdog_status();
    let quota = read_codex_quota()?;
    let reset = quota
        .resets_at_unix_seconds
        .map(reset_summary)
        .unwrap_or_else(|| "reset unknown".to_owned());
    println!("Weekly remaining: {}%", quota.remaining_percent);
    println!("Resets in: {reset}");
    println!("Status: {}", status.label());
    Ok(())
}

fn start_watchdog(proxy: EventLoopProxy<UserEvent>) {
    thread::spawn(move || {
        loop {
            let _ = proxy.send_event(UserEvent::Status(read_watchdog_status()));
            thread::sleep(Duration::from_secs(3));
        }
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
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("codex.exe")
        })
        .unwrap_or(false)
}

fn most_recent_session_file() -> Option<PathBuf> {
    let root = std::env::var_os("USERPROFILE")?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    find_session_files(
        &Path::new(&root).join(".codex").join("sessions"),
        &mut newest,
    );
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
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified())
            && newest
                .as_ref()
                .is_none_or(|(current, _)| modified > *current)
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
    file.seek(SeekFrom::Start(length.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut tail = String::new();
    file.read_to_string(&mut tail).ok()?;
    tail.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            event
                .get("payload")?
                .get("type")?
                .as_str()
                .map(str::to_owned)
        })
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
            "Codex: {}% left | reset {} | {}",
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
                let line =
                    line.map_err(|error| format!("Could not read Codex response: {error}"))?;
                let message: Value = serde_json::from_str(&line)
                    .map_err(|error| format!("Invalid Codex response: {error}"))?;
                match message.get("id").and_then(Value::as_i64) {
                    Some(0) => {
                        write_json_line(
                            &mut stdin,
                            json!({ "method": "initialized", "params": {} }),
                        )?;
                        write_json_line(
                            &mut stdin,
                            json!({ "method": "account/rateLimits/read", "id": 1 }),
                        )?;
                        Ok(None)
                    }
                    Some(1) if message.get("error").is_some() => Err(format!(
                        "Codex rejected usage request: {}",
                        message["error"]
                    )),
                    Some(1) => parse_weekly_quota(&line)
                        .map(Some)
                        .map_err(|error| error.to_string()),
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
            "Could not find native codex.exe. Install Codex or set CODEX_MONITOR_CODEX_PATH."
                .to_owned()
        })
}

fn write_json_line(writer: &mut impl Write, message: Value) -> Result<(), String> {
    writeln!(writer, "{message}")
        .map_err(|error| format!("Could not write Codex request: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("Could not send Codex request: {error}"))
}

fn update_tray(tray: &TrayIcon, state: &AppState) {
    if let Ok(icon) = render_gauge_icon(state.remaining_percent, state.status) {
        let _ = tray.set_icon(Some(icon));
    }
    let _ = tray.set_tooltip(Some(state.tooltip()));
}

fn render_gauge_icon(
    remaining_percent: u8,
    status: MonitorStatus,
) -> Result<Icon, tray_icon::BadIcon> {
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
            (
                start.0 + (end.0 - start.0) * t,
                start.1 + (end.1 - start.1) * t,
            ),
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
    format!("{days}d {hours}h")
}

fn reset_absolute(resets_at_unix_seconds: i64) -> String {
    Local
        .timestamp_opt(resets_at_unix_seconds, 0)
        .single()
        .map(|time| time.format("%a, %d %b %Y · %H:%M %Z").to_string())
        .unwrap_or_else(|| "Date unavailable".to_owned())
}
