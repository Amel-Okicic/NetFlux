//! egui/eframe front-end for NetFlux.
//!
//! Consumes `Snapshot`s from the backend (~1/sec) and emits `Command`s back.
//! This file is built against the shared contract in `crate::types` and must
//! not depend on any backend internals beyond the `snapshot_rx` receiver.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use eframe::egui::Widget;
use serde::{Deserialize, Serialize};

use crate::backend::rules;
use crate::backend::synthetic::{is_shapable, is_system};
use crate::types::{Command, GlobalLimits, ProcessStats, Rule, RuleBackup, Snapshot};
use crate::update_check::{self, UpdateInfo};

/// How many total-rate samples we keep for the header graph.
const TOTAL_HISTORY_LEN: usize = 60;

/// Project links used by the Help menu and the About dialog.
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const DISPLAY_VERSION: &str = "1.0";
/// Version of the bundled WinDivert driver (see `vendor/`).
const WINDIVERT_VERSION: &str = "2.2.2";

const COLOR_DOWN: egui::Color32 = egui::Color32::from_rgb(0x3b, 0x9d, 0xff);
const COLOR_UP: egui::Color32 = egui::Color32::from_rgb(0xff, 0x4d, 0x5e);
const COLOR_BLOCK: egui::Color32 = egui::Color32::from_rgb(0xff, 0xb4, 0x5e);
const COLOR_ACCENT: egui::Color32 = egui::Color32::from_rgb(0xd1, 0xd5, 0xdb);
const COLOR_MUTED: egui::Color32 = egui::Color32::from_rgb(0x9c, 0xa3, 0xaf);
const COLOR_LIGHT_GREY: egui::Color32 = egui::Color32::from_rgb(0xd3, 0xd3, 0xd3);
const COLOR_CARD: egui::Color32 = egui::Color32::from_rgb(0x1a, 0x1a, 0x1d);
const COLOR_BORDER: egui::Color32 = egui::Color32::from_rgb(0x32, 0x32, 0x38);

// ---------------------------------------------------------------------------
// Formatting / parsing helpers
// ---------------------------------------------------------------------------

const KB: f64 = 1024.0;
const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const TB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

/// Humanize a byte-per-second rate using base-1024 units.
fn humanize_rate(bps: u64) -> String {
    let b = bps as f64;
    if b >= GB {
        format!("{:.2} GB/s", b / GB)
    } else if b >= MB {
        format!("{:.2} MB/s", b / MB)
    } else if b >= KB {
        format!("{:.1} KB/s", b / KB)
    } else {
        format!("{bps} B/s")
    }
}

/// Humanize a plain byte total using base-1024 units.
fn humanize_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= TB {
        format!("{:.2} TB", b / TB)
    } else if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Rate unit for the limit dialog's dropdown (base 1024).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RateUnit {
    Bps,
    KBps,
    MBps,
    GBps,
}

impl RateUnit {
    const ALL: [RateUnit; 4] = [
        RateUnit::Bps,
        RateUnit::KBps,
        RateUnit::MBps,
        RateUnit::GBps,
    ];

    fn label(self) -> &'static str {
        match self {
            RateUnit::Bps => "B/s",
            RateUnit::KBps => "KB/s",
            RateUnit::MBps => "MB/s",
            RateUnit::GBps => "GB/s",
        }
    }

    fn multiplier(self) -> f64 {
        match self {
            RateUnit::Bps => 1.0,
            RateUnit::KBps => KB,
            RateUnit::MBps => MB,
            RateUnit::GBps => GB,
        }
    }

    /// Split a byte rate into (value, unit) using the largest unit >= 1.
    fn decompose(bps: u64) -> (f64, RateUnit) {
        let b = bps as f64;
        if b >= GB {
            (b / GB, RateUnit::GBps)
        } else if b >= MB {
            (b / MB, RateUnit::MBps)
        } else if b >= KB {
            (b / KB, RateUnit::KBps)
        } else {
            (b, RateUnit::Bps)
        }
    }
}

fn unix_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_remaining_time(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {seconds:02}s")
    }
}

/// One-line summary of a rule for the table column.
fn rule_summary(rule: Option<&Rule>) -> String {
    let Some(rule) = rule else {
        return "-".to_string();
    };

    let mut parts = Vec::new();
    if rule.blocked {
        parts.push("BLOCKED".to_string());
    } else {
        if let Some(down) = rule.down_limit {
            parts.push(format!("Dn {}", humanize_rate(down)));
        }
        if let Some(up) = rule.up_limit {
            parts.push(format!("Up {}", humanize_rate(up)));
        }
    }

    if let Some(expires_at) = rule.expires_at {
        let remaining = format!(
            "{} left",
            format_remaining_time(expires_at.saturating_sub(unix_time_secs()))
        );
        if parts.is_empty() {
            parts.push("Temporary".to_string());
        }
        parts.insert(0, remaining);
    }

    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" · ")
    }
}

// ---------------------------------------------------------------------------
// Sorting / modal state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SortColumn {
    Name,
    Flows,
    TotalTraffic,
    Down,
    Up,
    DownTotal,
    UpTotal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
enum TableColumn {
    Process,
    Download,
    Upload,
    TotalDown,
    TotalUp,
    TotalTraffic,
    Flows,
    Policy,
}

impl TableColumn {
    const DEFAULT_ORDER: [Self; 8] = [
        Self::Process,
        Self::Download,
        Self::Upload,
        Self::TotalDown,
        Self::TotalUp,
        Self::TotalTraffic,
        Self::Flows,
        Self::Policy,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Process => "Process",
            Self::Download => "Download",
            Self::Upload => "Upload",
            Self::TotalDown => "Total down",
            Self::TotalUp => "Total up",
            Self::TotalTraffic => "Total traffic",
            Self::Flows => "Flows",
            Self::Policy => "Policy",
        }
    }

    fn sort_column(self) -> Option<SortColumn> {
        match self {
            Self::Process => Some(SortColumn::Name),
            Self::Download => Some(SortColumn::Down),
            Self::Upload => Some(SortColumn::Up),
            Self::TotalDown => Some(SortColumn::DownTotal),
            Self::TotalUp => Some(SortColumn::UpTotal),
            Self::TotalTraffic => Some(SortColumn::TotalTraffic),
            Self::Flows => Some(SortColumn::Flows),
            Self::Policy => None,
        }
    }

    fn min_width(self) -> f32 {
        match self {
            Self::Process => 175.0,
            Self::Policy => 170.0,
            _ => 110.0,
        }
    }

    fn initial_width(self) -> f32 {
        match self {
            Self::Process => 300.0,
            Self::Policy => 180.0,
            _ => 110.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProcessFilter {
    All,
    Active,
    Limited,
    Blocked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LimitDuration {
    Permanent,
    ThirtyMinutes,
    OneHour,
    TwoHours,
    Custom,
    PreserveExisting,
}

/// State for the custom computer-wide bandwidth editor.
struct GlobalLimitModal {
    down_value: String,
    down_unit: RateUnit,
    up_value: String,
    up_unit: RateUnit,
    error: Option<String>,
}

/// State for the small "set limit" modal window.
struct LimitModal {
    exe_paths: Vec<String>,
    name: String,
    /// Empty values mean unlimited.
    down_value: String,
    down_unit: RateUnit,
    up_value: String,
    up_unit: RateUnit,
    duration: LimitDuration,
    custom_hours: String,
    custom_minutes: String,
    error: Option<String>,
}

/// A paired download/upload preset. An absent rate means unlimited.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LimitPreset {
    name: String,
    down_limit: Option<u64>,
    up_limit: Option<u64>,
}

/// User-facing preferences stored in %APPDATA%\\NetFlux\\settings.json.
/// The optional preset field allows migration from the older presets.json file.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct UiSettings {
    sort_column: SortColumn,
    sort_desc: bool,
    table_columns: Vec<TableColumn>,
    search_query: String,
    process_filter: ProcessFilter,
    close_to_tray: bool,
    presets: Option<Vec<LimitPreset>>,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            sort_column: SortColumn::Down,
            sort_desc: true,
            table_columns: TableColumn::DEFAULT_ORDER.to_vec(),
            search_query: String::new(),
            process_filter: ProcessFilter::All,
            close_to_tray: false,
            presets: None,
        }
    }
}

/// Editor state for adding or changing a custom preset.
struct PresetEditor {
    index: Option<usize>,
    name: String,
    down_value: String,
    up_value: String,
    error: Option<String>,
}

fn default_limit_presets() -> Vec<LimitPreset> {
    vec![
        LimitPreset {
            name: "Background".to_string(),
            down_limit: Some((0.5 * MB) as u64),
            up_limit: Some((0.05 * MB).round() as u64),
        },
        LimitPreset {
            name: "Normal".to_string(),
            down_limit: Some((5.0 * MB) as u64),
            up_limit: Some(MB as u64),
        },
        LimitPreset {
            name: "Unrestricted".to_string(),
            down_limit: None,
            up_limit: None,
        },
    ]
}

/// Load user-defined paired presets from the normal NetFlux configuration folder.
fn load_limit_presets() -> Vec<LimitPreset> {
    let path = rules::config_dir().join("presets.json");
    match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<Vec<LimitPreset>>(&data) {
            Ok(presets) => presets
                .into_iter()
                .filter(|preset| !preset.name.trim().is_empty())
                .collect(),
            Err(error) => {
                tracing::warn!(
                    "failed to parse {}: {error}; using built-in bandwidth presets",
                    path.display()
                );
                default_limit_presets()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => default_limit_presets(),
        Err(error) => {
            tracing::warn!(
                "failed to read {}: {error}; using built-in bandwidth presets",
                path.display()
            );
            default_limit_presets()
        }
    }
}

/// Persist paired presets independently from active process rules.
fn save_limit_presets(presets: &[LimitPreset]) -> Result<(), String> {
    let directory = rules::config_dir();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create the NetFlux data folder: {error}"))?;
    let path = directory.join("presets.json");
    let data = serde_json::to_string_pretty(presets)
        .map_err(|error| format!("Could not serialize custom presets: {error}"))?;
    std::fs::write(&path, data)
        .map_err(|error| format!("Could not save {}: {error}", path.display()))
}

/// Remove invalid, duplicate, or missing column entries from a settings file.
fn normalize_table_columns(columns: Vec<TableColumn>) -> Vec<TableColumn> {
    let mut normalized = Vec::with_capacity(TableColumn::DEFAULT_ORDER.len());
    for column in columns {
        if TableColumn::DEFAULT_ORDER.contains(&column) && !normalized.contains(&column) {
            normalized.push(column);
        }
    }
    for column in TableColumn::DEFAULT_ORDER {
        if !normalized.contains(&column) {
            normalized.push(column);
        }
    }
    normalized
}

/// Load GUI preferences. Older installations are migrated from presets.json.
fn load_ui_settings() -> UiSettings {
    let path = rules::config_dir().join("settings.json");
    let mut settings = match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<UiSettings>(&data) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(
                    "failed to parse {}: {error}; using default UI preferences",
                    path.display()
                );
                UiSettings::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => UiSettings::default(),
        Err(error) => {
            tracing::warn!(
                "failed to read {}: {error}; using default UI preferences",
                path.display()
            );
            UiSettings::default()
        }
    };

    settings.table_columns = normalize_table_columns(settings.table_columns);
    if settings.presets.is_none() {
        settings.presets = Some(load_limit_presets());
    }
    settings
}

/// Persist the complete GUI preference snapshot.
fn save_ui_settings(settings: &UiSettings) -> Result<(), String> {
    let directory = rules::config_dir();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create the NetFlux data folder: {error}"))?;
    let path = directory.join("settings.json");
    let data = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("Could not serialize UI preferences: {error}"))?;
    std::fs::write(&path, data)
        .map_err(|error| format!("Could not save {}: {error}", path.display()))
}

/// Convert the preset editor's MB/s value to bytes/sec. Blank means unlimited.
fn parse_preset_rate(value: &str) -> Result<Option<u64>, ()> {
    let trimmed = value.trim().replace(',', ".");
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mbps = trimmed.parse::<f64>().map_err(|_| ())?;
    let bytes_per_second = mbps * MB;
    if !mbps.is_finite()
        || mbps < 0.0
        || !bytes_per_second.is_finite()
        || bytes_per_second > u64::MAX as f64
    {
        return Err(());
    }
    Ok(Some(bytes_per_second.round() as u64))
}

/// Format a stored byte rate as a compact MB/s value for the preset editor.
fn preset_rate_value(limit: Option<u64>) -> String {
    let Some(limit) = limit else {
        return String::new();
    };
    let mut value = format!("{:.6}", limit as f64 / MB);
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value
}

/// Human-readable rate shown in the custom preset manager.
fn preset_rate_label(limit: Option<u64>) -> String {
    match limit {
        Some(_) => format!("{} MB/s", preset_rate_value(limit)),
        None => "Unlimited".to_string(),
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// The app
// ---------------------------------------------------------------------------

fn draw_direction_icon(ui: &mut egui::Ui, _color: egui::Color32, download: bool) {
    let (uri, bytes): (&'static str, &'static [u8]) = if download {
        (
            "bytes://netflux-download-icon.svg",
            include_bytes!("../assets/DownloadIco.svg"),
        )
    } else {
        (
            "bytes://netflux-upload-icon.svg",
            include_bytes!("../assets/UploadIco.svg"),
        )
    };
    ui.add(egui::Image::from_bytes(uri, bytes).fit_to_exact_size(egui::vec2(64.0, 55.0)));
}

fn draw_asset_icon(ui: &mut egui::Ui, asset: &'static str, bytes: &'static [u8], size: f32) {
    ui.add(egui::Image::from_bytes(asset, bytes).fit_to_exact_size(egui::vec2(size, size)));
}

fn draw_speedometer_gauge(
    ui: &mut egui::Ui,
    title: &str,
    rate_bps: u64,
    peak_bps: u64,
    color: egui::Color32,
) {
    let width = ui.available_width();
    let height = if title == "TOTAL" { 252.0 } else { 126.0 };
    let scale = height / 126.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

    let painter = ui.painter();
    let radius = if title == "TOTAL" {
        (rect.width() * 0.50).min(height * 0.52).clamp(38.0, 125.0)
    } else {
        (rect.width() * 0.42).min(height * 0.40).clamp(38.0, 52.0)
    };
    let center = egui::pos2(rect.center().x, rect.top() + height * 0.72);
    let scale_bps = (peak_bps.max(rate_bps).max(1_000_000) as f64 * 1.25).max(1_000_000.0);
    let fraction = (rate_bps as f64 / scale_bps).clamp(0.0, 1.0) as f32;

    painter.text(
        egui::pos2(rect.center().x, rect.top() + 6.0),
        egui::Align2::CENTER_TOP,
        title,
        egui::FontId::proportional(11.0),
        color,
    );

    let arc_points = |end_fraction: f32| -> Vec<egui::Pos2> {
        let steps = ((end_fraction * 64.0).ceil() as usize).max(2);
        (0..=steps)
            .map(|step| {
                let t = end_fraction * step as f32 / steps as f32;
                let angle = std::f32::consts::PI + std::f32::consts::PI * t;
                egui::pos2(
                    center.x + radius * angle.cos(),
                    center.y + radius * angle.sin(),
                )
            })
            .collect()
    };

    // The TOTAL dial keeps its existing diameter; its heavier ring provides
    // the visual weight shown in the reference layout.
    let arc_thickness = if title == "TOTAL" { 22.0 } else { 9.0 };
    painter.add(egui::Shape::line(
        arc_points(1.0),
        egui::Stroke::new(arc_thickness, egui::Color32::from_rgb(0x30, 0x35, 0x42)),
    ));

    if fraction > 0.001 {
        let active_points = arc_points(fraction);
        painter.add(egui::Shape::line(
            active_points.clone(),
            egui::Stroke::new(arc_thickness, color),
        ));
        if let Some(endpoint) = active_points.last() {
            painter.circle_filled(*endpoint, 6.0 * scale.min(1.5), color);
            painter.circle_stroke(
                *endpoint,
                8.0 * scale.min(1.5),
                egui::Stroke::new(
                    1.5 * scale.min(1.5),
                    egui::Color32::from_rgb(0x17, 0x1a, 0x24),
                ),
            );
        }
    }

    let mbps = rate_bps as f64 * 8.0 / 1_000_000.0;
    let max_mbps = scale_bps * 8.0 / 1_000_000.0;
    painter.text(
        egui::pos2(center.x, center.y - 2.0),
        egui::Align2::CENTER_CENTER,
        format!("{mbps:.1}"),
        egui::FontId::proportional(23.0 * scale.min(1.5)),
        egui::Color32::WHITE,
    );
    painter.text(
        egui::pos2(center.x, center.y + 20.0 * scale.min(1.5)),
        egui::Align2::CENTER_CENTER,
        "Mbps",
        egui::FontId::proportional(10.0 * scale.min(1.5)),
        COLOR_MUTED,
    );
    painter.text(
        egui::pos2(center.x - radius - 2.0, center.y + 9.0 * scale.min(1.5)),
        egui::Align2::LEFT_CENTER,
        "0",
        egui::FontId::proportional(9.0),
        COLOR_MUTED,
    );
    painter.text(
        egui::pos2(center.x + radius + 2.0, center.y + 9.0 * scale.min(1.5)),
        egui::Align2::RIGHT_CENTER,
        if max_mbps < 10.0 {
            format!("{max_mbps:.1}")
        } else {
            format!("{max_mbps:.0}")
        },
        egui::FontId::proportional(9.0),
        COLOR_MUTED,
    );
}

fn smooth_curve(points: &[egui::Pos2]) -> Vec<egui::Pos2> {
    if points.len() < 3 {
        return points.to_vec();
    }

    const STEPS_PER_SEGMENT: usize = 4;
    let mut smooth = Vec::with_capacity((points.len() - 1) * STEPS_PER_SEGMENT + 1);

    for index in 0..points.len() - 1 {
        let p0 = points[index.saturating_sub(1)];
        let p1 = points[index];
        let p2 = points[index + 1];
        let p3 = points[(index + 2).min(points.len() - 1)];

        let tangent1 = (p2.y - p0.y) * 0.5;
        let tangent2 = (p3.y - p1.y) * 0.5;

        for step in 0..STEPS_PER_SEGMENT {
            let t = step as f32 / STEPS_PER_SEGMENT as f32;
            let t2 = t * t;
            let t3 = t2 * t;

            let y = (2.0 * t3 - 3.0 * t2 + 1.0) * p1.y
                + (t3 - 2.0 * t2 + t) * tangent1
                + (-2.0 * t3 + 3.0 * t2) * p2.y
                + (t3 - t2) * tangent2;

            smooth.push(egui::pos2(
                egui::lerp(p1.x..=p2.x, t),
                y.clamp(p1.y.min(p2.y), p1.y.max(p2.y)),
            ));
        }
    }

    smooth.push(*points.last().expect("points is not empty"));
    smooth
}

fn draw_area_fill(
    painter: &egui::Painter,
    points: &[egui::Pos2],
    baseline: f32,
    color: egui::Color32,
) {
    if points.len() < 2 {
        return;
    }

    let top_color = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 42);
    let bottom_color = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 2);

    let mut mesh = egui::Mesh::default();
    mesh.reserve_vertices((points.len() - 1) * 4);
    mesh.reserve_triangles((points.len() - 1) * 2);

    for pair in points.windows(2) {
        let first = mesh.vertices.len() as u32;
        mesh.colored_vertex(pair[0], top_color);
        mesh.colored_vertex(pair[1], top_color);
        mesh.colored_vertex(egui::pos2(pair[1].x, baseline), bottom_color);
        mesh.colored_vertex(egui::pos2(pair[0].x, baseline), bottom_color);
        mesh.add_triangle(first, first + 1, first + 2);
        mesh.add_triangle(first, first + 2, first + 3);
    }

    painter.add(egui::Shape::mesh(mesh));
}

fn traffic_chart_points(
    history: &[f64],
    plot_rect: egui::Rect,
    axis_max_mbps: f64,
) -> Vec<egui::Pos2> {
    let count = history.len().min(TOTAL_HISTORY_LEN);
    if count == 0 {
        return Vec::new();
    }

    let start = history.len() - count;
    let first_slot = TOTAL_HISTORY_LEN - count;
    let denominator = (TOTAL_HISTORY_LEN - 1).max(1) as f32;

    history[start..]
        .iter()
        .enumerate()
        .map(|(index, rate_bps)| {
            let slot = first_slot + index;
            let x = plot_rect.left() + plot_rect.width() * slot as f32 / denominator;
            let rate_mbps = rate_bps * 8.0 / 1_000_000.0;
            let y = plot_rect.bottom()
                - plot_rect.height() * (rate_mbps / axis_max_mbps).clamp(0.0, 1.0) as f32;
            egui::pos2(x, y)
        })
        .collect()
}

fn draw_chart_value_label(
    painter: &egui::Painter,
    plot_rect: egui::Rect,
    point: egui::Pos2,
    rate_bps: u64,
    color: egui::Color32,
    above: bool,
) {
    let value_mbps = rate_bps as f64 * 8.0 / 1_000_000.0;
    let label = format!("{value_mbps:.2} Mbps");
    let label_size = egui::vec2(label.len() as f32 * 6.1 + 10.0, 18.0);
    let desired = egui::pos2(
        point.x + 8.0,
        if above {
            point.y - label_size.y - 5.0
        } else {
            point.y + 7.0
        },
    );
    let label_rect = egui::Rect::from_min_size(
        egui::pos2(
            desired.x.clamp(
                plot_rect.left(),
                (plot_rect.right() - label_size.x).max(plot_rect.left()),
            ),
            desired.y.clamp(
                plot_rect.top(),
                (plot_rect.bottom() - label_size.y).max(plot_rect.top()),
            ),
        ),
        label_size,
    );

    painter.rect_filled(
        label_rect,
        egui::CornerRadius::same(4),
        egui::Color32::from_rgb(0x0d, 0x12, 0x1c),
    );
    painter.rect_stroke(
        label_rect,
        egui::CornerRadius::same(4),
        egui::Stroke::new(1.0, color),
        egui::StrokeKind::Inside,
    );
    painter.text(
        label_rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(10.0),
        color,
    );
}

fn draw_network_chart(
    ui: &mut egui::Ui,
    down_history: &[f64],
    up_history: &[f64],
    down_now: u64,
    up_now: u64,
) {
    let (allocated_rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 264.0),
        egui::Sense::hover(),
    );

    let plot_rect = egui::Rect::from_min_max(
        egui::pos2(allocated_rect.left() + 66.0, allocated_rect.top() + 9.0),
        egui::pos2(
            allocated_rect.right() - 12.0,
            allocated_rect.bottom() - 31.0,
        ),
    );

    let maximum_bps = down_history
        .iter()
        .chain(up_history.iter())
        .copied()
        .fold(down_now.max(up_now) as f64, f64::max);
    let maximum_mbps = maximum_bps * 8.0 / 1_000_000.0;
    let raw_axis_max = (maximum_mbps * 1.12).max(1.0);
    let raw_step = raw_axis_max / 5.0;
    let magnitude = 10.0_f64.powf(raw_step.log10().floor());
    let normalized_step = raw_step / magnitude;
    let step_factor = if normalized_step <= 1.0 {
        1.0
    } else if normalized_step <= 2.0 {
        2.0
    } else if normalized_step <= 2.5 {
        2.5
    } else if normalized_step <= 5.0 {
        5.0
    } else {
        10.0
    };
    let step = step_factor * magnitude;
    let axis_max_mbps = ((raw_axis_max / step).ceil() * step).max(step);

    let painter = ui.painter();
    painter.rect_filled(
        plot_rect,
        egui::CornerRadius::same(7),
        egui::Color32::from_rgb(0x13, 0x17, 0x22),
    );

    for tick in 0..=5 {
        let fraction = tick as f32 / 5.0;
        let y = plot_rect.bottom() - plot_rect.height() * fraction;
        let value = axis_max_mbps * fraction as f64;
        let label = if axis_max_mbps < 10.0 {
            format!("{value:.1} Mbps")
        } else {
            format!("{value:.0} Mbps")
        };

        painter.line_segment(
            [
                egui::pos2(plot_rect.left(), y),
                egui::pos2(plot_rect.right(), y),
            ],
            egui::Stroke::new(
                if tick == 0 { 1.0 } else { 0.7 },
                egui::Color32::from_rgba_unmultiplied(82, 101, 133, 75),
            ),
        );
        painter.text(
            egui::pos2(plot_rect.left() - 9.0, y),
            egui::Align2::RIGHT_CENTER,
            label,
            egui::FontId::proportional(10.0),
            COLOR_MUTED,
        );
    }

    let time_labels = ["-60s", "-45s", "-30s", "-15s", "Now"];
    for (index, label) in time_labels.iter().enumerate() {
        let fraction = index as f32 / (time_labels.len() - 1) as f32;
        let x = plot_rect.left() + plot_rect.width() * fraction;

        if index > 0 && index < time_labels.len() - 1 {
            painter.line_segment(
                [
                    egui::pos2(x, plot_rect.top()),
                    egui::pos2(x, plot_rect.bottom()),
                ],
                egui::Stroke::new(0.7, egui::Color32::from_rgba_unmultiplied(82, 101, 133, 55)),
            );
        }

        painter.text(
            egui::pos2(x, plot_rect.bottom() + 7.0),
            egui::Align2::CENTER_TOP,
            *label,
            egui::FontId::proportional(10.0),
            COLOR_MUTED,
        );
    }

    let down_points = traffic_chart_points(down_history, plot_rect, axis_max_mbps);
    let up_points = traffic_chart_points(up_history, plot_rect, axis_max_mbps);
    let down_smooth = smooth_curve(&down_points);
    let up_smooth = smooth_curve(&up_points);
    let plot_painter = painter.with_clip_rect(plot_rect);

    // Draw download first and upload second so the smaller red area stays visible.
    draw_area_fill(&plot_painter, &down_smooth, plot_rect.bottom(), COLOR_DOWN);
    draw_area_fill(&plot_painter, &up_smooth, plot_rect.bottom(), COLOR_UP);

    if down_smooth.len() > 1 {
        plot_painter.add(egui::Shape::line(
            down_smooth,
            egui::Stroke::new(2.7, COLOR_DOWN),
        ));
    } else if let Some(point) = down_points.first() {
        plot_painter.circle_filled(*point, 2.5, COLOR_DOWN);
    }

    if up_smooth.len() > 1 {
        plot_painter.add(egui::Shape::line(
            up_smooth,
            egui::Stroke::new(2.3, COLOR_UP),
        ));
    } else if let Some(point) = up_points.first() {
        plot_painter.circle_filled(*point, 2.5, COLOR_UP);
    }

    if let Some(point) = down_points.last() {
        plot_painter.circle_filled(*point, 4.6, COLOR_DOWN);
        plot_painter.circle_stroke(*point, 5.2, egui::Stroke::new(1.5, egui::Color32::WHITE));
    }
    if let Some(point) = up_points.last() {
        plot_painter.circle_filled(*point, 4.0, COLOR_UP);
        plot_painter.circle_stroke(*point, 4.7, egui::Stroke::new(1.3, egui::Color32::WHITE));
    }

    if let Some(pointer) = response.hover_pos()
        && plot_rect.contains(pointer)
    {
        let fraction = ((pointer.x - plot_rect.left()) / plot_rect.width()).clamp(0.0, 1.0);
        let slot = (fraction * (TOTAL_HISTORY_LEN - 1) as f32).round() as usize;
        let count = down_history.len().min(TOTAL_HISTORY_LEN);
        let first_slot = TOTAL_HISTORY_LEN - count;

        if slot >= first_slot && slot < TOTAL_HISTORY_LEN {
            let index = slot - first_slot;
            let history_index = down_history.len() - count + index;
            let x =
                plot_rect.left() + plot_rect.width() * slot as f32 / (TOTAL_HISTORY_LEN - 1) as f32;

            plot_painter.line_segment(
                [
                    egui::pos2(x, plot_rect.top()),
                    egui::pos2(x, plot_rect.bottom()),
                ],
                egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(210, 220, 240, 125),
                ),
            );

            let down_rate = down_history[history_index] as u64;
            let up_rate = up_history.get(history_index).copied().unwrap_or_default() as u64;

            if let Some(point) = down_points.get(index) {
                plot_painter.circle_filled(*point, 4.2, COLOR_DOWN);
                plot_painter.circle_stroke(
                    *point,
                    4.8,
                    egui::Stroke::new(1.2, egui::Color32::WHITE),
                );
                draw_chart_value_label(
                    &plot_painter,
                    plot_rect,
                    *point,
                    down_rate,
                    COLOR_DOWN,
                    true,
                );
            }
            if let Some(point) = up_points.get(index) {
                plot_painter.circle_filled(*point, 4.0, COLOR_UP);
                plot_painter.circle_stroke(
                    *point,
                    4.6,
                    egui::Stroke::new(1.2, egui::Color32::WHITE),
                );
                draw_chart_value_label(&plot_painter, plot_rect, *point, up_rate, COLOR_UP, false);
            }

            let _ = response.on_hover_text(format!(
                "Download: {}\nUpload: {}",
                humanize_rate(down_rate),
                humanize_rate(up_rate),
            ));
        }
    }
}

fn draw_dialog_header(
    ui: &mut egui::Ui,
    title: &str,
    close_id: &str,
    close_clicked: &mut bool,
    corner_radius: u8,
) {
    let width = ui.available_width().max(1.0);
    let (header_rect, _) = ui.allocate_exact_size(egui::vec2(width, 46.0), egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(
        header_rect,
        egui::CornerRadius {
            nw: corner_radius,
            ne: corner_radius,
            sw: 0,
            se: 0,
        },
        egui::Color32::from_rgb(0x55, 0x50, 0xc8),
    );

    let close_rect = egui::Rect::from_center_size(
        egui::pos2(header_rect.right() - 23.0, header_rect.center().y),
        egui::vec2(30.0, 30.0),
    );
    let close_response = ui.interact(
        close_rect,
        ui.make_persistent_id(close_id),
        egui::Sense::click(),
    );

    if close_response.hovered() {
        painter.rect_filled(
            close_rect,
            egui::CornerRadius::same(7),
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 32),
        );
    }

    let cross = close_rect.shrink(11.0);
    let cross_stroke = egui::Stroke::new(1.5, egui::Color32::WHITE);
    painter.line_segment([cross.left_top(), cross.right_bottom()], cross_stroke);
    painter.line_segment([cross.right_top(), cross.left_bottom()], cross_stroke);

    let title_rect = egui::Rect::from_min_max(
        egui::pos2(header_rect.left() + 18.0, header_rect.top()),
        egui::pos2(close_rect.left() - 10.0, header_rect.bottom()),
    );
    painter.with_clip_rect(title_rect).text(
        title_rect.left_center(),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::proportional(16.0),
        egui::Color32::WHITE,
    );

    if close_response.clicked() {
        *close_clicked = true;
    }
}

fn apply_dark_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = egui::Color32::from_rgb(0x1b, 0x19, 0x35);
    visuals.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0x65, 0x6c, 0xd9));
    visuals.panel_fill = egui::Color32::from_rgb(0x12, 0x11, 0x16);
    visuals.extreme_bg_color = egui::Color32::from_rgb(0x0d, 0x0d, 0x10);
    visuals.faint_bg_color = egui::Color32::from_rgb(0x22, 0x22, 0x27);
    visuals.hyperlink_color = COLOR_ACCENT;
    visuals.selection.bg_fill = egui::Color32::from_rgb(0x2b, 0x43, 0x69);
    // Rounded glass-inspired controls: indigo surfaces, cool highlights and soft borders.
    visuals.widgets.noninteractive.bg_fill = COLOR_CARD;
    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x2b, 0x2b, 0x69);
    visuals.widgets.inactive.bg_stroke =
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x65, 0x6c, 0xd9));
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x43, 0x45, 0xa0);
    visuals.widgets.hovered.bg_stroke =
        egui::Stroke::new(1.2, egui::Color32::from_rgb(0xa5, 0xa9, 0xff));
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0x55, 0x50, 0xc8);
    visuals.widgets.active.bg_stroke =
        egui::Stroke::new(1.4, egui::Color32::from_rgb(0xc0, 0xc2, 0xff));
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(18);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(18);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(18);
    ctx.set_visuals(visuals);
}

pub struct NetFluxApp {
    snapshot_rx: Receiver<Snapshot>,
    cmd_tx: Sender<Command>,

    last_snapshot: Snapshot,
    total_down_hist: Vec<f64>,
    total_up_hist: Vec<f64>,

    sort_column: SortColumn,
    sort_desc: bool,
    table_columns: Vec<TableColumn>,
    selected: Option<String>,
    /// Processes selected for bulk actions.
    selected_processes: HashSet<String>,
    /// Anchor used by Shift-click range selection.
    selection_anchor: Option<String>,
    /// Last visible process order; holding Ctrl preserves this order while rates update.
    frozen_order: Vec<String>,
    process_icons: HashMap<String, egui::TextureHandle>,
    search_query: String,
    process_filter: ProcessFilter,

    limit_modal: Option<LimitModal>,
    global_limit_modal: Option<GlobalLimitModal>,
    pending_global_limits: Option<GlobalLimits>,
    presets: Vec<LimitPreset>,
    preset_manager_open: bool,
    preset_editor: Option<PresetEditor>,
    preset_error: Option<String>,
    process_action_window: Option<(String, String)>,
    about_open: bool,
    close_to_tray: bool,
    tray_icon: Option<tray_icon::TrayIcon>,
    force_exit: bool,
    shutdown_sent: bool,

    update_rx: Receiver<UpdateInfo>,
    update: Option<UpdateInfo>,
    update_dismissed: bool,
    update_install: UpdateInstall,
}

/// Progress of an in-app update started from the banner.
enum UpdateInstall {
    Idle,
    Running(Receiver<Result<std::path::PathBuf, String>>),
    Failed(String),
}

impl NetFluxApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        snapshot_rx: Receiver<Snapshot>,
        cmd_tx: Sender<Command>,
    ) -> Self {
        apply_dark_theme(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let mut style = (*cc.egui_ctx.global_style()).clone();
        style.spacing.item_spacing = egui::vec2(9.0, 7.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        cc.egui_ctx.set_global_style(style);

        let settings = load_ui_settings();
        let mut app = Self {
            snapshot_rx,
            cmd_tx,
            last_snapshot: Snapshot::default(),
            total_down_hist: Vec::with_capacity(TOTAL_HISTORY_LEN),
            total_up_hist: Vec::with_capacity(TOTAL_HISTORY_LEN),
            sort_column: settings.sort_column,
            sort_desc: settings.sort_desc,
            table_columns: settings.table_columns,
            selected: None,
            selected_processes: HashSet::new(),
            selection_anchor: None,
            frozen_order: Vec::new(),
            process_icons: HashMap::new(),
            search_query: settings.search_query,
            process_filter: settings.process_filter,
            limit_modal: None,
            global_limit_modal: None,
            pending_global_limits: None,
            presets: settings.presets.unwrap_or_else(load_limit_presets),
            preset_manager_open: false,
            preset_editor: None,
            preset_error: None,
            process_action_window: None,
            about_open: false,
            close_to_tray: settings.close_to_tray,
            tray_icon: None,
            force_exit: false,
            shutdown_sent: false,
            update_rx: update_check::spawn(APP_VERSION),
            update: None,
            update_dismissed: false,
            update_install: UpdateInstall::Idle,
        };

        // Restore the tray icon with its saved preference. A failure should
        // not leave the UI showing an enabled state without an actual icon.
        if app.close_to_tray
            && let Err(error) = app.enable_tray_icon(&cc.egui_ctx)
        {
            tracing::warn!("could not restore close-to-tray preference: {error}");
            app.close_to_tray = false;
            app.persist_ui_settings();
        }

        app
    }

    fn enable_tray_icon(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let icon =
            tray_icon::Icon::from_rgba(crate::icon::rgba(), crate::icon::SIZE, crate::icon::SIZE)
                .map_err(|error| format!("could not create tray icon: {error}"))?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_tooltip("NetFlux")
            .with_icon(icon)
            .build()
            .map_err(|error| format!("could not create tray icon: {error}"))?;

        let ctx = ctx.clone();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |event| match event {
            tray_icon::TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                button_state: tray_icon::MouseButtonState::Up,
                ..
            }
            | tray_icon::TrayIconEvent::DoubleClick {
                button: tray_icon::MouseButton::Left,
                ..
            } => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            }
            _ => {}
        }));

        self.tray_icon = Some(tray);
        Ok(())
    }

    fn send_shutdown(&mut self) {
        if !self.shutdown_sent {
            let _ = self.cmd_tx.send(Command::Shutdown);
            self.shutdown_sent = true;
        }
    }

    /// The current rule for `key`, or a fresh default rule bound to that path.
    fn base_rule(&self, key: &str) -> Rule {
        self.last_snapshot
            .rules
            .get(key)
            .cloned()
            .unwrap_or_else(|| Rule {
                exe_path: key.to_string(),
                ..Rule::default()
            })
    }

    fn send_rule(&mut self, rule: Rule) {
        let _ = self.cmd_tx.send(Command::SetRule(rule));
    }

    fn set_blocked(&mut self, key: &str, blocked: bool) {
        let mut rule = self.base_rule(key);
        rule.exe_path = key.to_string();
        rule.blocked = blocked;
        if rule.expires_at.is_some() {
            let backup = rule.restore.get_or_insert(RuleBackup {
                down_limit: None,
                up_limit: None,
                blocked,
            });
            backup.blocked = blocked;
        }
        self.send_rule(rule);
    }

    fn select_process_from_row(
        &mut self,
        key: &str,
        visible_keys: &[String],
        modifiers: egui::Modifiers,
    ) {
        if modifiers.shift {
            if let Some(anchor) = self.selection_anchor.as_deref() {
                let anchor_index = visible_keys.iter().position(|item| item == anchor);
                let target_index = visible_keys.iter().position(|item| item == key);
                if let (Some(start), Some(end)) = (anchor_index, target_index) {
                    if !modifiers.ctrl {
                        self.selected_processes.clear();
                    }
                    for item in &visible_keys[start.min(end)..=start.max(end)] {
                        self.selected_processes.insert(item.clone());
                    }
                } else {
                    if !modifiers.ctrl {
                        self.selected_processes.clear();
                    }
                    self.selected_processes.insert(key.to_string());
                    self.selection_anchor = Some(key.to_string());
                }
            } else {
                if !modifiers.ctrl {
                    self.selected_processes.clear();
                }
                self.selected_processes.insert(key.to_string());
                self.selection_anchor = Some(key.to_string());
            }
        } else if modifiers.ctrl {
            if !self.selected_processes.remove(key) {
                self.selected_processes.insert(key.to_string());
            }
            self.selection_anchor = Some(key.to_string());
        } else {
            self.selected_processes.clear();
            self.selected_processes.insert(key.to_string());
            self.selection_anchor = Some(key.to_string());
        }

        self.selected = if self.selected_processes.contains(key) {
            Some(key.to_string())
        } else {
            None
        };
    }

    fn selected_process_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.selected_processes.iter().cloned().collect();
        paths.sort_unstable();
        paths
    }

    fn apply_bulk_blocked(&mut self, paths: &[String], blocked: bool) {
        for key in paths {
            if !blocked && !self.last_snapshot.rules.contains_key(key) {
                continue;
            }
            self.set_blocked(key, blocked);
        }
    }

    fn apply_bulk_remove_rules(&mut self, paths: &[String]) {
        for key in paths {
            let _ = self.cmd_tx.send(Command::RemoveRule(key.clone()));
        }
    }

    fn open_limit_modal(&mut self, key: &str, name: &str) {
        self.open_limit_modal_for_paths(vec![key.to_string()], name.to_string());
    }

    fn open_limit_modal_for_paths(&mut self, exe_paths: Vec<String>, name: String) {
        let Some(first_path) = exe_paths.first() else {
            return;
        };
        let existing = self.last_snapshot.rules.get(first_path);
        let editable = |bps: Option<u64>| match bps {
            Some(bps) => {
                let (value, unit) = RateUnit::decompose(bps);
                let value = format!("{value:.2}");
                (
                    value
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                        .to_string(),
                    unit,
                )
            }
            None => (String::new(), RateUnit::MBps),
        };
        let (down_value, down_unit) = editable(existing.and_then(|r| r.down_limit));
        let (up_value, up_unit) = editable(existing.and_then(|r| r.up_limit));
        let is_bulk = exe_paths.len() > 1;
        let (duration, custom_hours, custom_minutes) = if is_bulk {
            (
                LimitDuration::PreserveExisting,
                String::new(),
                String::new(),
            )
        } else {
            existing
                .and_then(|rule| rule.expires_at)
                .filter(|expires_at| *expires_at > unix_time_secs())
                .map(|expires_at| {
                    let remaining = expires_at.saturating_sub(unix_time_secs());
                    let remaining_minutes =
                        (remaining / 60) + if remaining % 60 == 0 { 0 } else { 1 };
                    (
                        LimitDuration::Custom,
                        (remaining_minutes / 60).to_string(),
                        (remaining_minutes % 60).to_string(),
                    )
                })
                .unwrap_or_else(|| (LimitDuration::Permanent, String::new(), String::new()))
        };
        self.limit_modal = Some(LimitModal {
            exe_paths,
            name,
            down_value,
            down_unit,
            up_value,
            up_unit,
            duration,
            custom_hours,
            custom_minutes,
            error: None,
        });
    }

    fn open_global_limit_modal(&mut self) {
        let limits = self
            .pending_global_limits
            .unwrap_or(self.last_snapshot.global_limits);
        let editable = |bps: Option<u64>| match bps {
            Some(bps) => {
                let (value, unit) = RateUnit::decompose(bps);
                let value = format!("{value:.2}");
                (
                    value
                        .trim_end_matches('0')
                        .trim_end_matches('.')
                        .to_string(),
                    unit,
                )
            }
            None => (String::new(), RateUnit::MBps),
        };
        let (down_value, down_unit) = editable(limits.down_limit);
        let (up_value, up_unit) = editable(limits.up_limit);
        self.global_limit_modal = Some(GlobalLimitModal {
            down_value,
            down_unit,
            up_value,
            up_unit,
            error: None,
        });
    }

    fn open_preset_editor(&mut self, index: Option<usize>) {
        let preset = index.and_then(|index| self.presets.get(index));
        self.preset_editor = Some(PresetEditor {
            index,
            name: preset.map(|preset| preset.name.clone()).unwrap_or_default(),
            down_value: preset
                .map(|preset| preset_rate_value(preset.down_limit))
                .unwrap_or_default(),
            up_value: preset
                .map(|preset| preset_rate_value(preset.up_limit))
                .unwrap_or_default(),
            error: None,
        });
    }

    fn persist_ui_settings(&self) {
        let settings = UiSettings {
            sort_column: self.sort_column,
            sort_desc: self.sort_desc,
            table_columns: self.table_columns.clone(),
            search_query: self.search_query.clone(),
            process_filter: self.process_filter,
            close_to_tray: self.close_to_tray,
            presets: Some(self.presets.clone()),
        };
        if let Err(error) = save_ui_settings(&settings) {
            tracing::error!("{error}");
        }
    }

    fn persist_limit_presets(&mut self) {
        // Keep presets.json in sync for compatibility with older NetFlux builds.
        let result = save_limit_presets(&self.presets);
        self.preset_error = result.err();
        self.persist_ui_settings();
        if let Some(error) = &self.preset_error {
            tracing::error!("{error}");
        }
    }

    fn send_global_limits(&mut self, limits: GlobalLimits) {
        self.pending_global_limits = Some(limits);
        let _ = self.cmd_tx.send(Command::SetGlobalLimits(limits));
    }

    /// Draw a sortable, draggable table header cell.
    ///
    /// Dropping on the left or right half of a header inserts the dragged
    /// column before or after that target column.
    fn draw_table_header(
        &mut self,
        ui: &mut egui::Ui,
        column: TableColumn,
    ) -> Option<(TableColumn, TableColumn, bool)> {
        let sort_column = column.sort_column();
        let selected = sort_column.is_some_and(|sort| self.sort_column == sort);
        let direction = if selected {
            if self.sort_desc { "  v" } else { "  ^" }
        } else {
            ""
        };
        let label = format!("{}{direction}", column.label());

        let size = ui.available_size_before_wrap();
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());

        let background = if response.hovered() {
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 22)
        } else if selected {
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 12)
        } else {
            egui::Color32::TRANSPARENT
        };
        if background != egui::Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 0.0, background);
        }

        ui.painter().text(
            egui::pos2(rect.left() + 7.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.0),
            if selected {
                COLOR_LIGHT_GREY
            } else {
                COLOR_MUTED
            },
        );

        response.dnd_set_drag_payload(column);
        if response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        if response.dnd_hover_payload::<TableColumn>().is_some() {
            let pointer_x = ui
                .ctx()
                .pointer_latest_pos()
                .map(|pointer| pointer.x)
                .unwrap_or(rect.center().x);
            let marker_x = if pointer_x >= rect.center().x {
                rect.right()
            } else {
                rect.left()
            };
            ui.painter().line_segment(
                [
                    egui::pos2(marker_x, rect.top() + 4.0),
                    egui::pos2(marker_x, rect.bottom() - 4.0),
                ],
                egui::Stroke::new(2.0, COLOR_ACCENT),
            );
        }

        if response.clicked()
            && let Some(sort) = sort_column
        {
            if self.sort_column == sort {
                self.sort_desc = !self.sort_desc;
            } else {
                self.sort_column = sort;
                self.sort_desc = true;
            }
            self.persist_ui_settings();
        }

        let _ = response.clone().on_hover_text(if sort_column.is_some() {
            "Click to sort · Drag to reorder"
        } else {
            "Drag to reorder"
        });

        if let Some(dragged) = response.dnd_release_payload::<TableColumn>() {
            let pointer_x = ui
                .ctx()
                .pointer_latest_pos()
                .map(|pointer| pointer.x)
                .unwrap_or(rect.center().x);
            return Some((*dragged, column, pointer_x >= rect.center().x));
        }

        None
    }

    fn move_table_column(&mut self, dragged: TableColumn, target: TableColumn, insert_after: bool) {
        let Some(from_index) = self
            .table_columns
            .iter()
            .position(|column| *column == dragged)
        else {
            return;
        };
        let Some(target_index) = self
            .table_columns
            .iter()
            .position(|column| *column == target)
        else {
            return;
        };

        if from_index == target_index {
            return;
        }

        let mut insert_index = target_index + if insert_after { 1 } else { 0 };
        let column = self.table_columns.remove(from_index);
        if from_index < insert_index {
            insert_index -= 1;
        }
        self.table_columns
            .insert(insert_index.min(self.table_columns.len()), column);
        self.persist_ui_settings();
    }

    fn draw_menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open rules file").clicked() {
                        open_path(&rules::rules_path());
                        ui.close();
                    }
                    if ui.button("Open settings file").clicked() {
                        open_path(&rules::config_dir().join("settings.json"));
                        ui.close();
                    }
                    if ui.button("Open log file").clicked() {
                        open_path(&rules::config_dir().join("netflux.log"));
                        ui.close();
                    }
                    if ui.button("Open data folder").clicked() {
                        open_path(&rules::config_dir());
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        self.force_exit = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        ui.close();
                    }
                });
                ui.menu_button("Custom presets", |ui| {
                    if ui.button("Manage presets...").clicked() {
                        self.preset_manager_open = true;
                        ui.close();
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("Documentation").clicked() {
                        open_url(ui.ctx(), &format!("{REPO_URL}#readme"));
                        ui.close();
                    }
                    if ui.button("Report issue").clicked() {
                        open_url(ui.ctx(), &format!("{REPO_URL}/issues/new"));
                        ui.close();
                    }
                    let update_label = match &self.update {
                        Some(u) => format!("Update available: v{}…", u.version),
                        None => "Check for updates…".to_string(),
                    };
                    if ui.button(update_label).clicked() {
                        open_url(ui.ctx(), &format!("{REPO_URL}/releases/latest"));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("View license").clicked() {
                        open_url(ui.ctx(), &format!("{REPO_URL}/blob/main/LICENSE"));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("About NetFlux").clicked() {
                        self.about_open = true;
                        ui.close();
                    }
                });
            });
        });
    }

    fn draw_preset_manager(&mut self, ctx: &egui::Context) {
        if !self.preset_manager_open {
            return;
        }

        let mut open = true;
        let mut close_clicked = false;
        let mut add_clicked = false;
        let mut edit_index = None;
        let mut delete_index = None;

        egui::Window::new("CUSTOM PRESETS")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x12, 0x11, 0x16))
                    .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                    .corner_radius(egui::CornerRadius::same(13))
                    .inner_margin(0.0),
            )
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(760.0)
            .min_width(760.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                draw_dialog_header(
                    ui,
                    "CUSTOM PRESETS",
                    "custom-presets-dialog-close",
                    &mut close_clicked,
                    11,
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Save paired download and upload limits for quick reuse.",
                            )
                            .size(13.0)
                            .color(COLOR_MUTED),
                        );
                        ui.add_space(16.0);

                        egui::Grid::new("custom-presets-table")
                            .num_columns(5)
                            .spacing([12.0, 10.0])
                            .striped(true)
                            .show(ui, |ui| {
                                ui.add_sized(
                                    [220.0, 26.0],
                                    egui::Label::new(
                                        egui::RichText::new("PRESET").strong().color(COLOR_MUTED),
                                    ),
                                );
                                ui.add_sized(
                                    [145.0, 26.0],
                                    egui::Label::new(
                                        egui::RichText::new("DOWNLOAD").strong().color(COLOR_MUTED),
                                    ),
                                );
                                ui.add_sized(
                                    [145.0, 26.0],
                                    egui::Label::new(
                                        egui::RichText::new("UPLOAD").strong().color(COLOR_MUTED),
                                    ),
                                );
                                ui.add_sized(
                                    [66.0, 26.0],
                                    egui::Label::new(
                                        egui::RichText::new("EDIT").strong().color(COLOR_MUTED),
                                    ),
                                );
                                ui.add_sized(
                                    [66.0, 26.0],
                                    egui::Label::new(
                                        egui::RichText::new("DELETE").strong().color(COLOR_MUTED),
                                    ),
                                );
                                ui.end_row();

                                for (index, preset) in self.presets.iter().enumerate() {
                                    ui.add_sized(
                                        [220.0, 34.0],
                                        egui::Label::new(
                                            egui::RichText::new(&preset.name).strong(),
                                        ),
                                    );
                                    ui.add_sized(
                                        [145.0, 34.0],
                                        egui::Label::new(preset_rate_label(preset.down_limit)),
                                    );
                                    ui.add_sized(
                                        [145.0, 34.0],
                                        egui::Label::new(preset_rate_label(preset.up_limit)),
                                    );
                                    if ui
                                        .add_sized([66.0, 30.0], egui::Button::new("Edit"))
                                        .clicked()
                                    {
                                        edit_index = Some(index);
                                    }
                                    if ui
                                        .add_sized([66.0, 30.0], egui::Button::new("Delete"))
                                        .clicked()
                                    {
                                        delete_index = Some(index);
                                    }
                                    ui.end_row();
                                }
                            });

                        if self.presets.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new("No saved presets. Add one to get started.")
                                    .color(COLOR_MUTED),
                            );
                        }

                        if let Some(error) = &self.preset_error {
                            ui.add_space(10.0);
                            ui.colored_label(COLOR_BLOCK, error);
                        }

                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_sized(
                                    [140.0, 40.0],
                                    egui::Button::new(
                                        egui::RichText::new("＋ Add preset").strong(),
                                    )
                                    .fill(egui::Color32::from_rgb(0x55, 0x50, 0xc8))
                                    .stroke(egui::Stroke::new(
                                        1.2,
                                        egui::Color32::from_rgb(0xc0, 0xc2, 0xff),
                                    ))
                                    .corner_radius(egui::CornerRadius::same(18)),
                                )
                                .clicked()
                            {
                                add_clicked = true;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_sized([112.0, 40.0], egui::Button::new("Close"))
                                        .clicked()
                                    {
                                        close_clicked = true;
                                    }
                                },
                            );
                        });
                    });
            });

        self.preset_manager_open = open && !close_clicked;

        if let Some(index) = delete_index.filter(|index| *index < self.presets.len()) {
            self.presets.remove(index);
            self.persist_limit_presets();
        } else if let Some(index) = edit_index {
            self.open_preset_editor(Some(index));
        } else if add_clicked {
            self.open_preset_editor(None);
        }
    }

    fn draw_preset_editor(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.preset_editor.take() else {
            return;
        };

        let mut open = true;
        let mut close_clicked = false;
        let mut save_clicked = false;
        let mut cancel_clicked = false;

        egui::Window::new("EDIT CUSTOM PRESET")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x12, 0x11, 0x16))
                    .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                    .corner_radius(egui::CornerRadius::same(13))
                    .inner_margin(0.0),
            )
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .min_width(560.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                draw_dialog_header(
                    ui,
                    if editor.index.is_some() {
                        "EDIT CUSTOM PRESET"
                    } else {
                        "ADD CUSTOM PRESET"
                    },
                    "custom-preset-editor-close",
                    &mut close_clicked,
                    11,
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("PRESET NAME")
                                .size(11.0)
                                .strong()
                                .color(COLOR_MUTED),
                        );
                        ui.add_space(6.0);
                        ui.add_sized(
                            [500.0, 40.0],
                            egui::TextEdit::singleline(&mut editor.name)
                                .hint_text("e.g. Background")
                                .horizontal_align(egui::Align::Center)
                                .vertical_align(egui::Align::Center),
                        );

                        ui.add_space(16.0);
                        ui.columns(2, |columns| {
                            columns[0].label(
                                egui::RichText::new("DOWNLOAD LIMIT (MB/s)")
                                    .size(11.0)
                                    .strong()
                                    .color(COLOR_DOWN),
                            );
                            columns[0].add_sized(
                                [230.0, 40.0],
                                egui::TextEdit::singleline(&mut editor.down_value)
                                    .hint_text("Unlimited")
                                    .horizontal_align(egui::Align::Center)
                                    .vertical_align(egui::Align::Center),
                            );
                            columns[1].label(
                                egui::RichText::new("UPLOAD LIMIT (MB/s)")
                                    .size(11.0)
                                    .strong()
                                    .color(COLOR_UP),
                            );
                            columns[1].add_sized(
                                [230.0, 40.0],
                                egui::TextEdit::singleline(&mut editor.up_value)
                                    .hint_text("Unlimited")
                                    .horizontal_align(egui::Align::Center)
                                    .vertical_align(egui::Align::Center),
                            );
                        });
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Leave either field empty for Unlimited.")
                                .size(12.0)
                                .color(COLOR_MUTED),
                        );

                        if let Some(error) = &editor.error {
                            ui.add_space(10.0);
                            ui.colored_label(COLOR_BLOCK, error);
                        }

                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_sized(
                                            [112.0, 40.0],
                                            egui::Button::new(egui::RichText::new("Save").strong())
                                                .fill(egui::Color32::from_rgb(0x55, 0x50, 0xc8))
                                                .stroke(egui::Stroke::new(
                                                    1.2,
                                                    egui::Color32::from_rgb(0xc0, 0xc2, 0xff),
                                                ))
                                                .corner_radius(egui::CornerRadius::same(18)),
                                        )
                                        .clicked()
                                    {
                                        save_clicked = true;
                                    }
                                    if ui
                                        .add_sized([112.0, 40.0], egui::Button::new("Cancel"))
                                        .clicked()
                                    {
                                        cancel_clicked = true;
                                    }
                                },
                            );
                        });
                    });
            });

        if cancel_clicked || close_clicked || !open {
            self.preset_editor = None;
            return;
        }

        if save_clicked {
            let name = editor.name.trim().to_string();
            if name.is_empty() {
                editor.error = Some("Enter a name for this preset.".to_string());
            } else if self.presets.iter().enumerate().any(|(index, preset)| {
                Some(index) != editor.index && preset.name.eq_ignore_ascii_case(&name)
            }) {
                editor.error = Some("A preset with that name already exists.".to_string());
            } else {
                match (
                    parse_preset_rate(&editor.down_value),
                    parse_preset_rate(&editor.up_value),
                ) {
                    (Ok(down_limit), Ok(up_limit)) => {
                        let preset = LimitPreset {
                            name,
                            down_limit,
                            up_limit,
                        };
                        if let Some(index) = editor.index {
                            if let Some(existing) = self.presets.get_mut(index) {
                                *existing = preset;
                            } else {
                                self.presets.push(preset);
                            }
                        } else {
                            self.presets.push(preset);
                        }
                        self.persist_limit_presets();
                        self.preset_editor = None;
                        return;
                    }
                    _ => {
                        editor.error = Some(
                            "Enter valid non-negative speeds, or leave a field empty for Unlimited."
                                .to_string(),
                        );
                    }
                }
            }
        }

        self.preset_editor = Some(editor);
    }

    fn draw_update_banner(&mut self, ui: &mut egui::Ui) {
        if self.update_dismissed {
            return;
        }
        let Some(update) = self.update.clone() else {
            return;
        };

        // Poll a running install; on success relaunch the new build and quit.
        if let UpdateInstall::Running(rx) = &self.update_install
            && let Ok(result) = rx.try_recv()
        {
            let relaunched = result.and_then(|exe| {
                std::process::Command::new(&exe)
                    .args(["--wait-pid", &std::process::id().to_string()])
                    .spawn()
                    .map_err(|e| format!("could not start the new version: {e}"))
            });
            self.update_install = match relaunched {
                Ok(_) => {
                    self.force_exit = true;
                    self.send_shutdown();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    UpdateInstall::Idle
                }
                Err(e) => {
                    tracing::error!("update failed: {e}");
                    UpdateInstall::Failed(e)
                }
            };
        }

        let release_page = if update.url.is_empty() {
            format!("{REPO_URL}/releases/latest")
        } else {
            update.url.clone()
        };

        egui::Panel::top("update_banner").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "NetFlux v{} is available (you have v{APP_VERSION}).",
                        update.version
                    ))
                    .strong(),
                );
                if let UpdateInstall::Running(_) = self.update_install {
                    ui.spinner();
                    ui.label("Downloading update…");
                    return;
                }
                if let UpdateInstall::Failed(e) = &self.update_install {
                    ui.label(egui::RichText::new(format!("Update failed: {e}")).color(COLOR_BLOCK));
                }
                match &update.asset_url {
                    Some(asset_url) => {
                        if ui.button("Update and restart").clicked() {
                            let (tx, rx) = crossbeam_channel::bounded(1);
                            let asset_url = asset_url.clone();
                            std::thread::spawn(move || {
                                let result =
                                    update_check::install(&asset_url).map_err(|e| format!("{e:#}"));
                                let _ = tx.send(result);
                            });
                            self.update_install = UpdateInstall::Running(rx);
                        }
                        if ui.button("Release notes").clicked() {
                            open_url(ui.ctx(), &release_page);
                        }
                    }
                    None => {
                        if ui.button("Download").clicked() {
                            open_url(ui.ctx(), &release_page);
                        }
                    }
                }
                if ui.button("Dismiss").clicked() {
                    self.update_dismissed = true;
                }
            });
        });
    }

    fn draw_about(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let mut open = true;
        let mut close_clicked = false;
        egui::Window::new("ABOUT NETFLUX")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x1b, 0x19, 0x35))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                    ))
                    .corner_radius(egui::CornerRadius::same(13))
                    .inner_margin(0.0),
            )
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                draw_dialog_header(
                    ui,
                    "ABOUT NETFLUX",
                    "about-dialog-close",
                    &mut close_clicked,
                    11,
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(15, 13))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(6.0);
                            ui.heading("NetFlux");
                            ui.label(
                                egui::RichText::new(format!("Version {DISPLAY_VERSION}")).strong(),
                            );
                            ui.add_space(4.0);
                            ui.label(env!("CARGO_PKG_DESCRIPTION"));
                            ui.add_space(8.0);
                        });
                        ui.separator();
                        egui::Grid::new("about_grid").num_columns(2).show(ui, |ui| {
                            ui.label("Packet driver");
                            ui.label(format!("WinDivert {WINDIVERT_VERSION}"));
                            ui.end_row();
                            ui.label("UI framework");
                            ui.label("egui / eframe");
                            ui.end_row();
                            ui.label("License");
                            ui.label("Multi-license");
                            ui.end_row();
                            ui.label("Data folder");
                            ui.label(
                                egui::RichText::new(rules::config_dir().display().to_string())
                                    .weak()
                                    .small(),
                            );
                            ui.end_row();
                        });
                        ui.separator();
                        ui.horizontal(|ui| {
                            ui.hyperlink_to("GitHub", REPO_URL);
                            ui.hyperlink_to("Releases", format!("{REPO_URL}/releases"));
                            ui.hyperlink_to("Report issue", format!("{REPO_URL}/issues"));
                            ui.hyperlink_to(
                                "Licensing",
                                format!("{REPO_URL}/blob/main/LICENSES.md"),
                            );
                        });
                        ui.add_space(4.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("Copyright © 2026 Amel Okicic")
                                    .weak()
                                    .small(),
                            );
                            ui.label(
                                egui::RichText::new(
                                    "Based on Throttle v0.1.2 by Francesco Gruosso",
                                )
                                .weak()
                                .small(),
                            );
                        });
                    });
            });
        self.about_open = open && !close_clicked;
    }

    #[allow(clippy::too_many_arguments)]
    fn metric_card(
        ui: &mut egui::Ui,
        title: &str,
        value: String,
        footer: String,
        tint: egui::Color32,
        symbol: Option<&str>,
        asset: Option<(&'static str, &'static [u8])>,
        right_lines: Vec<String>,
        rule_limits: Vec<(Option<String>, Option<String>, bool)>,
        clickable_lines: Vec<(String, String)>,
        right_side_lines: bool,
        sparkline: Option<(Vec<f64>, egui::Color32)>,
    ) {
        let width = ui.available_width();
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0x18, 0x1b, 0x26))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgb(0x2b, 0x31, 0x42),
            ))
            .corner_radius(egui::CornerRadius::same(13))
            .inner_margin(egui::Margin::symmetric(15, 13))
            .show(ui, |ui| {
                ui.set_min_width((width - 30.0).max(0.0));
                // Keep all four icons aligned with the card-title top edge,
                // using a consistent 50.4x50.4 footprint (20% larger).
                ui.set_min_height(109.2);
                ui.horizontal(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(58.0, 82.0),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            if let Some((uri, bytes)) = asset {
                                draw_asset_icon(ui, uri, bytes, 50.4);
                            } else if let Some(symbol) = symbol {
                                ui.add_sized(
                                    [50.4, 50.4],
                                    egui::Label::new(
                                        egui::RichText::new(symbol).size(33.6).strong().color(tint),
                                    ),
                                );
                            }
                        },
                    );
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new(title)
                                .size(11.0)
                                .strong()
                                .color(COLOR_MUTED),
                        );
                        ui.label(
                            egui::RichText::new(value)
                                .size(25.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );
                        ui.label(egui::RichText::new(footer).size(10.0).color(COLOR_MUTED));
                        if !right_side_lines {
                            for line in right_lines.iter().take(3) {
                                ui.label(egui::RichText::new(line).size(9.0).color(tint));
                            }
                            for (index, (key, name)) in clickable_lines.iter().take(3).enumerate() {
                                let response = ui.add(
                                    egui::Button::new(
                                        egui::RichText::new(if clickable_lines.len() == 1 {
                                            name.clone()
                                        } else {
                                            format!("{}. {}", index + 1, name)
                                        })
                                        .size(9.0)
                                        .color(tint),
                                    )
                                    .frame(false),
                                );
                                if response.clicked() {
                                    ui.ctx().data_mut(|data| {
                                        data.insert_temp(
                                            egui::Id::new("netflux-top-process-click"),
                                            (key.clone(), name.clone()),
                                        )
                                    });
                                }
                            }
                        }
                    });
                    if right_side_lines && (!right_lines.is_empty() || !clickable_lines.is_empty())
                    {
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            if title == "TRAFFIC RULES" {
                                egui::ScrollArea::vertical()
                                    .id_salt("traffic-rules-card-scroll")
                                    .max_height(109.2)
                                    .auto_shrink([false, true])
                                    .show(ui, |ui| {
                                        for line in right_lines.iter().take(2) {
                                            let line_color = if line.starts_with("GLOBAL DOWNLOAD")
                                            {
                                                COLOR_DOWN
                                            } else {
                                                COLOR_UP
                                            };
                                            ui.label(
                                                egui::RichText::new(line)
                                                    .size(9.0)
                                                    .color(line_color),
                                            );
                                        }
                                        if !right_lines.is_empty() {
                                            ui.add_space(9.0);
                                            let unlimit_clicked = ui
                                                .scope(|ui| {
                                                    let visuals = ui.visuals_mut();
                                                    visuals.widgets.inactive.weak_bg_fill =
                                                        egui::Color32::from_rgb(0xd3, 0xd3, 0xd3);
                                                    visuals.widgets.inactive.bg_stroke =
                                                        egui::Stroke::new(
                                                            1.0,
                                                            egui::Color32::from_rgb(
                                                                0x9a, 0xa1, 0xac,
                                                            ),
                                                        );
                                                    visuals.widgets.hovered.weak_bg_fill =
                                                        egui::Color32::from_rgb(0xe3, 0xe6, 0xeb);
                                                    visuals.widgets.hovered.bg_stroke =
                                                        egui::Stroke::new(
                                                            1.0,
                                                            egui::Color32::from_rgb(
                                                                0xc7, 0xcc, 0xd4,
                                                            ),
                                                        );
                                                    visuals.widgets.active.weak_bg_fill =
                                                        egui::Color32::from_rgb(0xf0, 0xf1, 0xf3);
                                                    visuals.widgets.inactive.fg_stroke =
                                                        egui::Stroke::new(
                                                            1.0,
                                                            egui::Color32::BLACK,
                                                        );
                                                    visuals.widgets.hovered.fg_stroke =
                                                        egui::Stroke::new(
                                                            1.0,
                                                            egui::Color32::BLACK,
                                                        );
                                                    visuals.widgets.active.fg_stroke =
                                                        egui::Stroke::new(
                                                            1.0,
                                                            egui::Color32::BLACK,
                                                        );

                                                    ui.add_sized(
                                                        [98.0, 30.0],
                                                        egui::Button::new(
                                                            egui::RichText::new("UNLIMIT")
                                                                .size(12.0)
                                                                .strong()
                                                                .color(egui::Color32::BLACK),
                                                        )
                                                        .corner_radius(egui::CornerRadius::same(3)),
                                                    )
                                                    .on_hover_text(
                                                        "Remove both global bandwidth limits",
                                                    )
                                                    .clicked()
                                                })
                                                .inner;
                                            if unlimit_clicked {
                                                ui.ctx().data_mut(|data| {
                                                    data.insert_temp(
                                                        egui::Id::new(
                                                            "netflux-global-unlimit-click",
                                                        ),
                                                        true,
                                                    )
                                                });
                                            }
                                        }
                                        for (index, (key, name)) in
                                            clickable_lines.iter().enumerate()
                                        {
                                            ui.horizontal(|ui| {
                                                let response = ui.add(
                                                    egui::Button::new(
                                                        egui::RichText::new(format!(
                                                            "{}. {}",
                                                            index + 1,
                                                            name
                                                        ))
                                                        .size(9.0)
                                                        .color(COLOR_LIGHT_GREY),
                                                    )
                                                    .frame(false),
                                                );
                                                if response.clicked() {
                                                    ui.ctx().data_mut(|data| {
                                                        data.insert_temp(
                                                            egui::Id::new(
                                                                "netflux-top-process-click",
                                                            ),
                                                            (key.clone(), name.clone()),
                                                        )
                                                    });
                                                }
                                                if let Some((down, up, blocked)) =
                                                    rule_limits.get(index)
                                                {
                                                    if *blocked {
                                                        draw_asset_icon(
                                                            ui,
                                                            "bytes://netflux-rule-blocked.svg",
                                                            include_bytes!(
                                                                "../assets/BlockedIcon.svg"
                                                            ),
                                                            13.0,
                                                        );
                                                    }
                                                    if let Some(rate) = down {
                                                        draw_asset_icon(
                                                            ui,
                                                            "bytes://netflux-rule-download.svg",
                                                            include_bytes!(
                                                                "../assets/DownloadIco.svg"
                                                            ),
                                                            13.0,
                                                        );
                                                        ui.label(
                                                            egui::RichText::new(rate)
                                                                .size(9.0)
                                                                .color(COLOR_LIGHT_GREY),
                                                        );
                                                    }
                                                    if let Some(rate) = up {
                                                        draw_asset_icon(
                                                            ui,
                                                            "bytes://netflux-rule-upload.svg",
                                                            include_bytes!(
                                                                "../assets/UploadIco.svg"
                                                            ),
                                                            13.0,
                                                        );
                                                        ui.label(
                                                            egui::RichText::new(rate)
                                                                .size(9.0)
                                                                .color(COLOR_LIGHT_GREY),
                                                        );
                                                    }
                                                }
                                            });
                                        }
                                    });
                            } else {
                                let line_color = if title == "PROCESSES" {
                                    egui::Color32::from_rgb(0xe5, 0xd5, 0x8f)
                                } else {
                                    tint
                                };
                                for (index, (key, name)) in
                                    clickable_lines.iter().take(5).enumerate()
                                {
                                    let response = ui.add(
                                        egui::Button::new(
                                            egui::RichText::new(format!("{}. {}", index + 1, name))
                                                .size(9.0)
                                                .color(line_color),
                                        )
                                        .frame(false),
                                    );
                                    if response.clicked() {
                                        ui.ctx().data_mut(|data| {
                                            data.insert_temp(
                                                egui::Id::new("netflux-top-process-click"),
                                                (key.clone(), name.clone()),
                                            )
                                        });
                                    }
                                }
                                for (index, line) in right_lines.iter().take(5).enumerate() {
                                    ui.label(
                                        egui::RichText::new(format!("{}. {}", index + 1, line))
                                            .size(9.0)
                                            .color(line_color),
                                    );
                                }
                            }
                        });
                    }
                    if let Some((samples, color)) = sparkline {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let (rect, _) = ui
                                .allocate_exact_size(egui::vec2(88.0, 52.0), egui::Sense::hover());
                            if samples.len() > 1 {
                                let max = samples.iter().copied().fold(1.0_f64, f64::max);
                                let points: Vec<egui::Pos2> = samples
                                    .iter()
                                    .enumerate()
                                    .map(|(i, v)| {
                                        let x = rect.left()
                                            + rect.width() * i as f32 / (samples.len() - 1) as f32;
                                        let y = rect.bottom()
                                            - rect.height() * (*v / max).clamp(0.0, 1.0) as f32;
                                        egui::pos2(x, y)
                                    })
                                    .collect();
                                ui.painter()
                                    .add(egui::Shape::line(points, egui::Stroke::new(1.8, color)));
                            }
                        });
                    }
                });
            });
    }

    fn draw_top_panel(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let active_count = snap
            .processes
            .values()
            .filter(|p| p.down_rate > 0 || p.up_rate > 0)
            .count();
        let global_limits = self.pending_global_limits.unwrap_or(snap.global_limits);
        let global_limit_count = (global_limits.down_limit.is_some() as usize)
            + (global_limits.up_limit.is_some() as usize);
        let blocked_count = snap.rules.values().filter(|r| r.blocked).count();
        let configured_rule_count = snap
            .rules
            .values()
            .filter(|rule| rule.is_configured())
            .count()
            + global_limit_count;
        let rules_footer = if global_limit_count > 0 {
            let label = if global_limit_count == 1 {
                "global limit"
            } else {
                "global limits"
            };
            format!("{blocked_count} blocked · {global_limit_count} {label}")
        } else {
            format!("{blocked_count} blocked")
        };
        let total_down: u64 = snap.processes.values().map(|p| p.down_total).sum();
        let total_up: u64 = snap.processes.values().map(|p| p.up_total).sum();
        let mut top_down: Vec<_> = snap
            .processes
            .iter()
            .filter(|(_, p)| p.down_rate > 0)
            .collect();
        top_down.sort_by_key(|(_, p)| std::cmp::Reverse(p.down_rate));
        let mut top_up: Vec<_> = snap
            .processes
            .iter()
            .filter(|(_, p)| p.up_rate > 0)
            .collect();
        top_up.sort_by_key(|(_, p)| std::cmp::Reverse(p.up_rate));
        let top_active: Vec<(String, String)> = {
            let mut active: Vec<_> = snap
                .processes
                .iter()
                .filter(|(_, p)| p.down_rate > 0 || p.up_rate > 0)
                .collect();
            active.sort_by_key(|(_, p)| std::cmp::Reverse(p.down_rate.saturating_add(p.up_rate)));
            active
                .into_iter()
                .take(3)
                .map(|(key, p)| (key.clone(), p.name.clone()))
                .collect()
        };
        let down_clickable: Vec<(String, String)> = top_down
            .iter()
            .take(1)
            .map(|(key, p)| ((*key).clone(), p.name.clone()))
            .collect();
        let up_clickable: Vec<(String, String)> = top_up
            .iter()
            .take(1)
            .map(|(key, p)| ((*key).clone(), p.name.clone()))
            .collect();
        let down_top = if down_clickable.is_empty() {
            "Top: no active traffic".to_string()
        } else {
            "Top download process:".to_string()
        };
        let up_top = if up_clickable.is_empty() {
            "Top: no active traffic".to_string()
        } else {
            "Top upload process:".to_string()
        };
        let mut configured_rules: Vec<_> = snap
            .rules
            .values()
            .filter(|rule| rule.is_configured())
            .collect();
        configured_rules.sort_by_key(|r| (!r.blocked, r.exe_path.to_lowercase()));
        let rule_clickable: Vec<(String, String)> = configured_rules
            .iter()
            .map(|rule| {
                let name = snap
                    .processes
                    .values()
                    .find(|p| p.exe_path.eq_ignore_ascii_case(&rule.exe_path))
                    .map(|p| p.name.clone())
                    .or_else(|| {
                        std::path::Path::new(&rule.exe_path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| rule.exe_path.clone());
                (rule.exe_path.clone(), name)
            })
            .collect();
        let rule_summaries: Vec<(Option<String>, Option<String>, bool)> = configured_rules
            .iter()
            .map(|rule| {
                (
                    rule.down_limit.map(humanize_rate),
                    rule.up_limit.map(humanize_rate),
                    rule.blocked,
                )
            })
            .collect();
        let mut global_rule_summaries = Vec::new();
        if let Some(limit) = global_limits.down_limit {
            global_rule_summaries.push(format!("GLOBAL DOWNLOAD: {}", humanize_rate(limit)));
        }
        if let Some(limit) = global_limits.up_limit {
            global_rule_summaries.push(format!("GLOBAL UPLOAD: {}", humanize_rate(limit)));
        }

        // Draw directly in the content area; a top panel adds a full-width
        // separator stroke at its lower edge.
        ui.add_space(7.0);
        {
            // Reserve the full header row first, then position each section
            // inside that fixed rectangle. The bandwidth controls are centered
            // in the true free span between the fixed logo and tray sections.
            let (header_rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 51.0), egui::Sense::hover());
            let mut header_ui = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("netflux-header-layout")
                    .max_rect(header_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );

            let logo_rect =
                egui::Rect::from_min_size(header_rect.left_top(), egui::vec2(180.0, 51.0));
            header_ui.put(
                logo_rect,
                egui::Image::from_bytes(
                    "bytes://netflux-logo.svg",
                    include_bytes!("../assets/Logo.svg"),
                )
                .fit_to_exact_size(egui::vec2(180.0, 51.0)),
            );

            // The tray section keeps its existing 104 px label, 8 px gap,
            // and 82 px toggle. Its right edge is fixed to the header's right.
            let tray_section_width = 194.0;
            let tray_left = header_rect.right() - tray_section_width;
            let center_rect = egui::Rect::from_min_max(
                egui::pos2(logo_rect.right(), header_rect.top()),
                egui::pos2(tray_left, header_rect.bottom()),
            );

            // The six controls occupy fixed widths totalling 544 px. Account
            // for the five normal item gaps and the existing 22 px separator,
            // then center that exact-width row in the available middle span.
            let control_spacing = header_ui.spacing().item_spacing.x;
            let control_group_width = 544.0 + 5.0 * control_spacing + 22.0;
            let control_group_rect = egui::Rect::from_center_size(
                center_rect.center(),
                egui::vec2(control_group_width, 51.0),
            );

            let effective_limits = global_limits;
            header_ui.scope_builder(
                egui::UiBuilder::new()
                    .id_salt("netflux-global-bandwidth-controls")
                    .max_rect(control_group_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(104.0, 32.0),
                        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        |ui| {
                            ui.label(
                                egui::RichText::new("MAX DOWNLOAD")
                                    .font(egui::FontId::proportional(14.0))
                                    .color(COLOR_MUTED),
                            );
                        },
                    );

                    let down_active = effective_limits.down_limit.is_some();
                    let down_button_text = effective_limits
                        .down_limit
                        .map(|limit| {
                            if limit == MB as u64 {
                                "1 MB/s".to_string()
                            } else {
                                humanize_rate(limit)
                            }
                        })
                        .unwrap_or_else(|| "1 MB/s".to_string());

                    if ui
                        .add_sized(
                            [92.0, 32.0],
                            egui::Button::new(
                                egui::RichText::new(down_button_text).size(12.0).strong(),
                            )
                            .fill(if down_active {
                                egui::Color32::from_rgb(0x55, 0x50, 0xc8)
                            } else {
                                egui::Color32::from_rgb(0x2b, 0x2b, 0x69)
                            })
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                            ))
                            .corner_radius(egui::CornerRadius::same(18)),
                        )
                        .clicked()
                    {
                        let mut limits = effective_limits;
                        limits.down_limit = if down_active { None } else { Some(MB as u64) };
                        self.send_global_limits(limits);
                    }

                    if ui
                        .add_sized(
                            [76.0, 32.0],
                            egui::Button::new(egui::RichText::new("CUSTOM").size(10.0).strong())
                                .fill(egui::Color32::from_rgb(0x17, 0x1a, 0x24))
                                .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                                .corner_radius(egui::CornerRadius::same(18)),
                        )
                        .clicked()
                    {
                        self.open_global_limit_modal();
                    }

                    ui.add_space(22.0);

                    ui.allocate_ui_with_layout(
                        egui::vec2(104.0, 32.0),
                        egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
                        |ui| {
                            ui.label(
                                egui::RichText::new("MAX UPLOAD")
                                    .font(egui::FontId::proportional(14.0))
                                    .color(COLOR_MUTED),
                            );
                        },
                    );

                    let up_active = effective_limits.up_limit.is_some();
                    let up_button_text = effective_limits
                        .up_limit
                        .map(|limit| {
                            if limit == (0.3 * MB) as u64 {
                                "0.3 MB/s".to_string()
                            } else {
                                humanize_rate(limit)
                            }
                        })
                        .unwrap_or_else(|| "0.3 MB/s".to_string());

                    if ui
                        .add_sized(
                            [92.0, 32.0],
                            egui::Button::new(
                                egui::RichText::new(up_button_text).size(12.0).strong(),
                            )
                            .fill(if up_active {
                                egui::Color32::from_rgb(0x55, 0x50, 0xc8)
                            } else {
                                egui::Color32::from_rgb(0x2b, 0x2b, 0x69)
                            })
                            .stroke(egui::Stroke::new(
                                1.0,
                                egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                            ))
                            .corner_radius(egui::CornerRadius::same(18)),
                        )
                        .clicked()
                    {
                        let mut limits = effective_limits;
                        limits.up_limit = if up_active {
                            None
                        } else {
                            Some((0.3 * MB) as u64)
                        };
                        self.send_global_limits(limits);
                    }

                    if ui
                        .add_sized(
                            [76.0, 32.0],
                            egui::Button::new(egui::RichText::new("CUSTOM").size(10.0).strong())
                                .fill(egui::Color32::from_rgb(0x17, 0x1a, 0x24))
                                .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                                .corner_radius(egui::CornerRadius::same(18)),
                        )
                        .clicked()
                    {
                        self.open_global_limit_modal();
                    }
                },
            );
            // Draw the tray controls at fixed coordinates so their right edge
            // remains anchored regardless of the bandwidth group's width.
            let label_rect = egui::Rect::from_min_size(
                egui::pos2(tray_left, header_rect.center().y - 16.0),
                egui::vec2(104.0, 32.0),
            );
            header_ui.painter().text(
                label_rect.center(),
                egui::Align2::CENTER_CENTER,
                "CLOSE TO TRAY",
                egui::FontId::proportional(14.0),
                COLOR_MUTED,
            );

            let toggle_rect = egui::Rect::from_min_size(
                egui::pos2(header_rect.right() - 82.0, header_rect.center().y - 16.0),
                egui::vec2(82.0, 32.0),
            );
            let toggle_response = header_ui
                .interact(
                    toggle_rect,
                    header_ui.make_persistent_id("close-to-tray-toggle"),
                    egui::Sense::click(),
                )
                .on_hover_text(
                    "ON: closing hides NetFlux in the system tray. OFF: closing exits NetFlux.",
                );
            let toggle_painter = header_ui.painter();
            let toggle_radius = toggle_rect.height() / 2.0;
            let knob_radius = 11.0;
            let knob_y = toggle_rect.center().y;
            if self.close_to_tray {
                toggle_painter.rect_filled(
                    toggle_rect,
                    egui::CornerRadius::same(16),
                    egui::Color32::from_rgb(0x55, 0x50, 0xC8),
                );
                toggle_painter.text(
                    egui::pos2(toggle_rect.left() + 25.0, knob_y),
                    egui::Align2::CENTER_CENTER,
                    "ON",
                    egui::FontId::proportional(14.0),
                    COLOR_LIGHT_GREY,
                );
                toggle_painter.circle_filled(
                    egui::pos2(toggle_rect.right() - toggle_radius, knob_y),
                    knob_radius,
                    egui::Color32::WHITE,
                );
            } else {
                toggle_painter.rect_filled(toggle_rect, egui::CornerRadius::same(16), COLOR_CARD);
                toggle_painter.rect_stroke(
                    toggle_rect,
                    egui::CornerRadius::same(16),
                    egui::Stroke::new(1.5, COLOR_MUTED),
                    egui::StrokeKind::Inside,
                );
                toggle_painter.circle_filled(
                    egui::pos2(toggle_rect.left() + toggle_radius, knob_y),
                    knob_radius,
                    egui::Color32::WHITE,
                );
                toggle_painter.text(
                    egui::pos2(toggle_rect.right() - 25.0, knob_y),
                    egui::Align2::CENTER_CENTER,
                    "OFF",
                    egui::FontId::proportional(14.0),
                    egui::Color32::WHITE,
                );
            }
            if toggle_response.clicked() {
                if self.close_to_tray {
                    self.close_to_tray = false;
                    self.tray_icon = None;
                    self.persist_ui_settings();
                } else {
                    match self.enable_tray_icon(header_ui.ctx()) {
                        Ok(()) => {
                            self.close_to_tray = true;
                            self.persist_ui_settings();
                        }
                        Err(error) => {
                            tracing::error!("{error}");
                            self.close_to_tray = false;
                        }
                    }
                }
            }
            ui.add_space(12.0);
            let old_item_spacing = ui.spacing().item_spacing;
            ui.spacing_mut().item_spacing.x = 16.0;
            ui.columns(4, |columns| {
                Self::metric_card(
                    &mut columns[0],
                    "DOWNLOAD",
                    humanize_rate(snap.total_down_rate),
                    format!("{} received", humanize_bytes(total_down)),
                    COLOR_DOWN,
                    None,
                    Some((
                        "bytes://netflux-card-download.svg",
                        include_bytes!("../assets/DownloadIco.svg"),
                    )),
                    vec![down_top],
                    vec![],
                    down_clickable,
                    false,
                    Some((self.total_down_hist.clone(), COLOR_DOWN)),
                );
                Self::metric_card(
                    &mut columns[1],
                    "UPLOAD",
                    humanize_rate(snap.total_up_rate),
                    format!("{} sent", humanize_bytes(total_up)),
                    COLOR_UP,
                    None,
                    Some((
                        "bytes://netflux-card-upload.svg",
                        include_bytes!("../assets/UploadIco.svg"),
                    )),
                    vec![up_top],
                    vec![],
                    up_clickable,
                    false,
                    Some((self.total_up_hist.clone(), COLOR_UP)),
                );
                Self::metric_card(
                    &mut columns[2],
                    "PROCESSES",
                    snap.processes.len().to_string(),
                    format!("{active_count} with active traffic"),
                    egui::Color32::from_rgb(0xaa, 0xb4, 0xd8),
                    None,
                    Some((
                        "bytes://netflux-card-process.svg",
                        include_bytes!("../assets/ProcessIcon.svg"),
                    )),
                    vec![],
                    vec![],
                    top_active,
                    true,
                    None,
                );
                Self::metric_card(
                    &mut columns[3],
                    "TRAFFIC RULES",
                    configured_rule_count.to_string(),
                    rules_footer,
                    egui::Color32::from_rgb(0x18, 0xc8, 0xe8),
                    None,
                    Some((
                        "bytes://netflux-card-rules.svg",
                        include_bytes!("../assets/TrafficRIcon.svg"),
                    )),
                    global_rule_summaries,
                    rule_summaries,
                    rule_clickable,
                    true,
                    None,
                );
            });
            ui.spacing_mut().item_spacing = old_item_spacing;
            let unlimit_clicked = ui.ctx().data_mut(|data| {
                data.remove_temp::<bool>(egui::Id::new("netflux-global-unlimit-click"))
            });
            if unlimit_clicked == Some(true) {
                self.send_global_limits(GlobalLimits::default());
            }
            if let Some((key, name)) = ui.ctx().data_mut(|data| {
                data.remove_temp::<(String, String)>(egui::Id::new("netflux-top-process-click"))
            }) {
                self.selected = Some(key.clone());
                self.process_action_window = Some((key, name));
            }
            ui.add_space(12.0);
            let traffic_card_width = (ui.available_width() - 5.0).max(0.0);
            egui::Frame::new()
                .fill(egui::Color32::from_rgb(0x17, 0x1a, 0x24))
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(0x2b, 0x31, 0x42),
                ))
                .corner_radius(egui::CornerRadius::same(13))
                .inner_margin(egui::Margin::symmetric(18, 15))
                .show(ui, |ui| {
                    // The frame adds 18 px of inner margin and a 1 px
                    // stroke on each side. Subtract all 38 px so the OUTER
                    // frame width exactly matches the dashboard content width.
                    ui.set_width((traffic_card_width - 38.0).max(0.0));

                    ui.horizontal(|ui| {
                        draw_asset_icon(
                            ui,
                            "bytes://netflux-traffic-header.svg",
                            include_bytes!("../assets/NetTrafIcon.svg"),
                            22.0,
                        );
                        ui.label(
                            egui::RichText::new("NETWORK TRAFFIC")
                                .size(13.0)
                                .strong()
                                .color(egui::Color32::WHITE),
                        );

                        let (live_rect, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter().circle_filled(
                            live_rect.center(),
                            4.0,
                            egui::Color32::from_rgb(0x22, 0xd3, 0x9a),
                        );
                        ui.label(
                            egui::RichText::new("Live")
                                .size(11.0)
                                .strong()
                                .color(egui::Color32::from_rgb(0x22, 0xd3, 0x9a)),
                        );

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new("LAST 60 SECONDS")
                                    .size(9.0)
                                    .strong()
                                    .color(COLOR_MUTED),
                            );
                        });
                    });

                    ui.add_space(12.0);

                    let total_rate = snap.total_down_rate.saturating_add(snap.total_up_rate);
                    let down_peak = self
                        .total_down_hist
                        .iter()
                        .copied()
                        .fold(snap.total_down_rate as f64, f64::max)
                        as u64;
                    let up_peak = self
                        .total_up_hist
                        .iter()
                        .copied()
                        .fold(snap.total_up_rate as f64, f64::max)
                        as u64;
                    let total_peak = self
                        .total_down_hist
                        .iter()
                        .zip(self.total_up_hist.iter())
                        .map(|(down, up)| (down + up) as u64)
                        .fold(total_rate, u64::max);
                    let section_width = ui.available_width();
                    // Use the same fixed gap between the three visual groups:
                    // TOTAL, the stacked directional gauges, and the chart.
                    let column_gap = 24.0;
                    let content_width = (section_width - column_gap).max(0.0);
                    let gauges_width = content_width * 0.30;
                    let chart_width = content_width - gauges_width;

                    // The two 24 px gaps below are the complete gaps;
                    // disable egui's automatic horizontal spacing here so it
                    // cannot silently add extra width and push the chart out.
                    let previous_section_spacing = ui.spacing().item_spacing.x;
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.horizontal(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(gauges_width, 252.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                let gauge_gap = 24.0;
                                let gauge_content_width = (gauges_width - gauge_gap).max(0.0);
                                // Give TOTAL the width needed for its broad dial;
                                // the narrower right column moves DOWNLOAD/UPLOAD
                                // toward the chart without enlarging their gauges.
                                let total_width = gauge_content_width * 0.77;
                                let small_width = gauge_content_width - total_width;

                                let previous_gauge_spacing = ui.spacing().item_spacing.x;
                                ui.spacing_mut().item_spacing.x = 0.0;
                                ui.horizontal(|ui| {
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(total_width, 252.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            draw_speedometer_gauge(
                                                ui,
                                                "TOTAL",
                                                total_rate,
                                                total_peak,
                                                egui::Color32::from_rgb(0xaa, 0xb4, 0xd8),
                                            );
                                        },
                                    );
                                    ui.add_space(gauge_gap);
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(small_width, 252.0),
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            draw_speedometer_gauge(
                                                ui,
                                                "DOWNLOAD",
                                                snap.total_down_rate,
                                                down_peak,
                                                COLOR_DOWN,
                                            );
                                            draw_speedometer_gauge(
                                                ui,
                                                "UPLOAD",
                                                snap.total_up_rate,
                                                up_peak,
                                                COLOR_UP,
                                            );
                                        },
                                    );
                                });
                                ui.spacing_mut().item_spacing.x = previous_gauge_spacing;
                            },
                        );
                        ui.add_space(column_gap);
                        ui.allocate_ui_with_layout(
                            egui::vec2(chart_width, 264.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                draw_network_chart(
                                    ui,
                                    &self.total_down_hist,
                                    &self.total_up_hist,
                                    snap.total_down_rate,
                                    snap.total_up_rate,
                                );
                            },
                        );
                    });
                    ui.spacing_mut().item_spacing.x = previous_section_spacing;
                });
        }
    }

    fn draw_bulk_action_toolbar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        if self.selected_processes.is_empty() {
            return;
        }

        let selected_paths = self.selected_process_paths();
        let manageable_paths: Vec<String> = selected_paths
            .iter()
            .filter(|key| is_shapable(key))
            .cloned()
            .collect();

        // "All" applies to the rows currently visible under the search and filter controls.
        let query = self.search_query.trim().to_lowercase();
        let visible_keys: Vec<String> = snap
            .processes
            .iter()
            .filter_map(|(key, stats)| {
                let matches_search = query.is_empty()
                    || stats.name.to_lowercase().contains(&query)
                    || key.to_lowercase().contains(&query);
                let rule = snap.rules.get(key);
                let matches_filter = match self.process_filter {
                    ProcessFilter::All => true,
                    ProcessFilter::Active => stats.down_rate > 0 || stats.up_rate > 0,
                    ProcessFilter::Limited => rule.is_some_and(|r| r.is_configured()),
                    ProcessFilter::Blocked => rule.is_some_and(|r| r.blocked),
                };
                (matches_search && matches_filter).then(|| key.clone())
            })
            .collect();

        let previous_spacing = ui.spacing().item_spacing;
        ui.spacing_mut().item_spacing.x = 4.0;

        ui.separator();
        ui.label(
            egui::RichText::new(format!("{} selected", selected_paths.len()))
                .size(10.0)
                .strong()
                .color(COLOR_LIGHT_GREY),
        );

        let compact_button =
            |ui: &mut egui::Ui, label: &str, width: f32, tooltip: &str, enabled: bool| {
                ui.add_enabled(
                    enabled,
                    egui::Button::new(egui::RichText::new(label).size(10.0).strong())
                        .min_size(egui::vec2(width, 32.0))
                        .fill(egui::Color32::from_rgb(0x2b, 0x2b, 0x69))
                        .stroke(egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                        ))
                        .corner_radius(egui::CornerRadius::same(14)),
                )
                .on_hover_text(tooltip)
                .clicked()
            };

        if compact_button(ui, "SELECT ALL", 78.0, "Select all visible processes", true) {
            self.selected_processes.extend(visible_keys.iter().cloned());
            if let Some(last) = visible_keys.last() {
                self.selection_anchor = Some(last.clone());
            }
        }
        if compact_button(ui, "DESELECT", 70.0, "Clear selection", true) {
            self.selected_processes.clear();
            self.selection_anchor = None;
            self.selected = None;
        }
        if ui
            .add_enabled(
                !manageable_paths.is_empty(),
                egui::Button::new(egui::RichText::new("LIMITS").size(10.0).strong())
                    .min_size(egui::vec2(54.0, 32.0))
                    .fill(egui::Color32::from_rgb(0x2b, 0x2b, 0x69))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                    ))
                    .corner_radius(egui::CornerRadius::same(14)),
            )
            .on_hover_text("Set download and upload limits")
            .clicked()
        {
            self.open_limit_modal_for_paths(
                manageable_paths.clone(),
                format!("{} selected processes", manageable_paths.len()),
            );
        }
        if compact_button(
            ui,
            "BLOCK",
            48.0,
            "Block selected processes",
            !manageable_paths.is_empty(),
        ) {
            self.apply_bulk_blocked(&manageable_paths, true);
        }
        if compact_button(
            ui,
            "UNBLOCK",
            62.0,
            "Unblock selected processes",
            !manageable_paths.is_empty(),
        ) {
            self.apply_bulk_blocked(&manageable_paths, false);
        }
        if compact_button(ui, "REMOVE", 56.0, "Remove selected processes' rules", true) {
            self.apply_bulk_remove_rules(&selected_paths);
        }

        ui.spacing_mut().item_spacing = previous_spacing;
    }

    fn draw_process_toolbar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            // Scale from the 300 px Process column at the default 1550 px
            // window size, accounting for the app's 39 px side gutters.
            let search_width = (ui.available_width()
                * (TableColumn::Process.initial_width() / 1472.0))
                .max(TableColumn::Process.min_width());

            let search_response = ui.add_sized(
                [search_width, 32.0],
                egui::TextEdit::singleline(&mut self.search_query)
                    .horizontal_align(egui::Align::Center)
                    .vertical_align(egui::Align::Center)
                    .hint_text("Search process or path"),
            );
            if search_response.changed() {
                self.persist_ui_settings();
            }

            ui.add_space(8.0);
            let filters = [
                (ProcessFilter::All, "ALL PROCESSES"),
                (ProcessFilter::Active, "ACTIVE TRAFFIC"),
                (ProcessFilter::Limited, "LIMITED"),
                (ProcessFilter::Blocked, "BLOCKED"),
            ];
            for (filter, label) in filters {
                let selected = self.process_filter == filter;
                let text = egui::RichText::new(label)
                    .size(11.0)
                    .strong()
                    .color(if selected {
                        egui::Color32::WHITE
                    } else {
                        COLOR_MUTED
                    });
                let button = egui::Button::new(text)
                    .min_size(egui::vec2(118.0, 32.0))
                    .fill(if selected {
                        egui::Color32::from_rgb(0x55, 0x50, 0xc8)
                    } else {
                        egui::Color32::from_rgb(0x2b, 0x2b, 0x69)
                    })
                    .stroke(egui::Stroke::new(
                        1.0,
                        if selected {
                            egui::Color32::from_rgb(0xc0, 0xc2, 0xff)
                        } else {
                            egui::Color32::from_rgb(0x65, 0x6c, 0xd9)
                        },
                    ))
                    .corner_radius(egui::CornerRadius::same(18));
                if ui.add(button).clicked() && self.process_filter != filter {
                    self.process_filter = filter;
                    self.persist_ui_settings();
                }
            }

            self.draw_bulk_action_toolbar(ui, snap);

            if self.selected_processes.is_empty() && ui.ctx().input(|i| i.modifiers.ctrl) {
                ui.label(
                    egui::RichText::new("ORDER FROZEN · CTRL")
                        .small()
                        .strong()
                        .color(COLOR_ACCENT),
                );
            }
        });
        ui.add_space(12.0);
    }

    fn draw_table(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        let mut rows: Vec<(String, ProcessStats)> = snap
            .processes
            .iter()
            .map(|(key, stats)| (key.clone(), stats.clone()))
            .collect();

        let query = self.search_query.trim().to_lowercase();
        let filter = self.process_filter;
        rows.retain(|(key, stats)| {
            let matches_search = query.is_empty()
                || stats.name.to_lowercase().contains(&query)
                || key.to_lowercase().contains(&query);
            let rule = snap.rules.get(key);
            let matches_filter = match filter {
                ProcessFilter::All => true,
                ProcessFilter::Active => stats.down_rate > 0 || stats.up_rate > 0,
                ProcessFilter::Limited => rule.is_some_and(|r| r.is_configured()),
                ProcessFilter::Blocked => rule.is_some_and(|r| r.blocked),
            };
            matches_search && matches_filter
        });

        let ctrl_held = ui.ctx().input(|i| i.modifiers.ctrl);
        if ctrl_held {
            for (key, _) in &rows {
                if !self.frozen_order.contains(key) {
                    self.frozen_order.push(key.clone());
                }
            }
            rows.sort_by_key(|(key, _)| {
                self.frozen_order
                    .iter()
                    .position(|saved| saved == key)
                    .unwrap_or(usize::MAX)
            });
        } else {
            let sort_column = self.sort_column;
            let sort_desc = self.sort_desc;
            rows.sort_by(|a, b| {
                let (a, b) = (&a.1, &b.1);
                let ord = match sort_column {
                    SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortColumn::Flows => a.flow_count.cmp(&b.flow_count),
                    SortColumn::TotalTraffic => a
                        .down_total
                        .saturating_add(a.up_total)
                        .cmp(&b.down_total.saturating_add(b.up_total)),
                    SortColumn::Down => a.down_rate.cmp(&b.down_rate),
                    SortColumn::Up => a.up_rate.cmp(&b.up_rate),
                    SortColumn::DownTotal => a.down_total.cmp(&b.down_total),
                    SortColumn::UpTotal => a.up_total.cmp(&b.up_total),
                };
                if sort_desc { ord.reverse() } else { ord }
            });
            self.frozen_order = rows.iter().map(|(key, _)| key.clone()).collect();
        }

        let visible_keys: Vec<String> = rows.iter().map(|(key, _)| key.clone()).collect();

        let columns = self.table_columns.clone();
        // TableBuilder stores column widths by position. Including the order in
        // its ID resets widths to their defaults after a reorder, so widths
        // never accidentally become attached to a different column.
        let order_key = columns.iter().fold(0_u64, |hash, column| {
            hash.wrapping_mul(31).wrapping_add(*column as u64 + 1)
        });
        let mut pending_reorder = None;

        let icon_ctx = ui.ctx().clone();
        let process_card_width = (ui.available_width() - 5.0).max(0.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0x17, 0x1a, 0x24))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgb(0x2b, 0x31, 0x42),
            ))
            .corner_radius(egui::CornerRadius::same(12))
            .inner_margin(egui::Margin::same(8))
            .show(ui, |ui| {
                // The frame adds 8 px of inner margin and a 1 px stroke
                // on each side. Subtract all 18 px so the OUTER frame width
                // matches the dashboard content width exactly.
                ui.set_width((process_card_width - 18.0).max(0.0));
                if rows.is_empty() {
                    ui.centered_and_justified(|ui| {
                        let message = if snap.processes.is_empty() {
                            "Waiting for process traffic…"
                        } else {
                            "No processes match this search or filter."
                        };
                        ui.label(egui::RichText::new(message).color(COLOR_MUTED));
                    });
                    return;
                }

                let mut table = egui_extras::TableBuilder::new(ui)
                    .id_salt(("netflux-process-table", order_key))
                    .resizable(true)
                    .auto_shrink(true)
                    .striped(true)
                    .sense(egui::Sense::CLICK | egui::Sense::HOVER);

                for (index, column) in columns.iter().enumerate() {
                    let is_last = index + 1 == columns.len();
                    let min_width = column.min_width();
                    let column_spec = if is_last {
                        // The final column absorbs the free space. With the
                        // default order this is Policy, keeping it flexible
                        // on the right as the window is resized.
                        egui_extras::Column::remainder()
                            .at_least(min_width)
                            .clip(true)
                    } else {
                        egui_extras::Column::initial(column.initial_width())
                            .at_least(min_width)
                            .clip(true)
                    };
                    table = table.column(column_spec);
                }

                table
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .header(30.0, |mut header| {
                        for column in &columns {
                            header.col(|ui| {
                                if let Some(reorder) = self.draw_table_header(ui, *column) {
                                    pending_reorder = Some(reorder);
                                }
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(48.0, rows.len(), |row| {
                            let (key, stats) = rows.get(row.index()).expect("row index in bounds");
                            let mut checkbox_clicked = false;
                            let response = self.draw_table_row(
                                row,
                                key,
                                stats,
                                snap,
                                &icon_ctx,
                                &columns,
                                &mut checkbox_clicked,
                            );
                            if response.clicked() && !checkbox_clicked {
                                let modifiers = icon_ctx.input(|input| input.modifiers);
                                self.select_process_from_row(key, &visible_keys, modifiers);
                            }
                            if response.secondary_clicked() {
                                self.selected = Some(key.clone());
                            }
                            self.row_context_menu(&response, key, &stats.name);
                        });
                    });
            });

        if let Some((dragged, target, insert_after)) = pending_reorder {
            self.move_table_column(dragged, target, insert_after);
        }
    }

    fn process_icon(&mut self, key: &str, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if let Some(icon) = self.process_icons.get(key) {
            return Some(icon.clone());
        }
        let image = extract_process_icon(key)?;
        let texture = ctx.load_texture(
            format!("process-icon:{key}"),
            image,
            egui::TextureOptions::LINEAR,
        );
        self.process_icons.insert(key.to_string(), texture.clone());
        Some(texture)
    }
    #[allow(clippy::too_many_arguments)]
    fn draw_table_row(
        &mut self,
        mut row: egui_extras::TableRow,
        key: &String,
        stats: &ProcessStats,
        snap: &Snapshot,
        ctx: &egui::Context,
        columns: &[TableColumn],
        checkbox_clicked: &mut bool,
    ) -> egui::Response {
        let selected =
            self.selected.as_deref() == Some(key.as_str()) || self.selected_processes.contains(key);

        let rule = snap.rules.get(key);
        let summary = rule_summary(rule);
        let rule_text = if rule.is_some_and(|r| r.blocked) {
            egui::RichText::new(summary).color(COLOR_BLOCK).strong()
        } else {
            egui::RichText::new(summary)
        };

        row.set_selected(selected);
        let icon = self.process_icon(key, ctx);

        for column in columns {
            match column {
                TableColumn::Process => {
                    row.col(|ui| {
                        ui.horizontal(|ui| {
                            let mut checked = self.selected_processes.contains(key);
                            if ui
                                .add_sized([20.0, 20.0], egui::Checkbox::new(&mut checked, ""))
                                .changed()
                            {
                                *checkbox_clicked = true;
                                if checked {
                                    self.selected_processes.insert(key.clone());
                                } else {
                                    self.selected_processes.remove(key);
                                    if self.selected.as_deref() == Some(key.as_str()) {
                                        self.selected = None;
                                    }
                                }
                                self.selection_anchor = Some(key.clone());
                            }

                            if let Some(icon) = &icon {
                                ui.add(
                                    egui::Image::new(icon)
                                        .fit_to_exact_size(egui::vec2(36.0, 36.0)),
                                );
                            } else {
                                ui.add_sized(
                                    [36.0, 36.0],
                                    egui::Label::new(egui::RichText::new("◈").color(COLOR_MUTED)),
                                );
                            }
                            egui::Label::new(egui::RichText::new(&stats.name).strong())
                                .wrap_mode(egui::TextWrapMode::Truncate)
                                .selectable(false)
                                .ui(ui);
                        });
                    });
                }
                TableColumn::Download => {
                    row.col(|ui| {
                        egui::Label::new(
                            egui::RichText::new(humanize_rate(stats.down_rate)).color(COLOR_DOWN),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .selectable(false)
                        .ui(ui);
                    });
                }
                TableColumn::Upload => {
                    row.col(|ui| {
                        egui::Label::new(
                            egui::RichText::new(humanize_rate(stats.up_rate)).color(COLOR_UP),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .selectable(false)
                        .ui(ui);
                    });
                }
                TableColumn::TotalDown => {
                    row.col(|ui| {
                        egui::Label::new(
                            egui::RichText::new(humanize_bytes(stats.down_total))
                                .color(COLOR_MUTED),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .selectable(false)
                        .ui(ui);
                    });
                }
                TableColumn::TotalUp => {
                    row.col(|ui| {
                        egui::Label::new(
                            egui::RichText::new(humanize_bytes(stats.up_total)).color(COLOR_MUTED),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .selectable(false)
                        .ui(ui);
                    });
                }
                TableColumn::TotalTraffic => {
                    row.col(|ui| {
                        egui::Label::new(
                            egui::RichText::new(humanize_bytes(
                                stats.down_total.saturating_add(stats.up_total),
                            ))
                            .color(COLOR_MUTED),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .selectable(false)
                        .ui(ui);
                    });
                }
                TableColumn::Flows => {
                    row.col(|ui| {
                        egui::Label::new(
                            egui::RichText::new(stats.flow_count.to_string()).color(COLOR_MUTED),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .selectable(false)
                        .ui(ui);
                    });
                }
                TableColumn::Policy => {
                    row.col(|ui| {
                        egui::Label::new(rule_text.clone())
                            .wrap_mode(egui::TextWrapMode::Truncate)
                            .selectable(false)
                            .ui(ui);
                    });
                }
            }
        }

        row.response()
    }

    fn row_context_menu(&mut self, resp: &egui::Response, key: &str, name: &str) {
        if resp.secondary_clicked() {
            self.selected = Some(key.to_string());
            self.process_action_window = Some((key.to_string(), name.to_string()));
        }
    }

    fn draw_process_action_window(&mut self, ctx: &egui::Context) {
        let Some((key, name)) = self.process_action_window.clone() else {
            return;
        };
        let mut open = true;
        let mut close_clicked = false;
        egui::Window::new("PROCESS ACTIONS")
            .id(egui::Id::new("netflux-process-actions"))
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x1b, 0x19, 0x35))
                    .stroke(egui::Stroke::new(
                        1.0,
                        egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                    ))
                    .corner_radius(egui::CornerRadius::same(20))
                    .inner_margin(0.0),
            )
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(420.0)
            .min_width(420.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                draw_dialog_header(
                    ui,
                    "PROCESS ACTIONS",
                    "process-actions-dialog-close",
                    &mut close_clicked,
                    18,
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        // Keep the entire process-actions window on the indigo glass palette.
                        let visuals = ui.visuals_mut();
                        visuals.window_fill = egui::Color32::from_rgb(0x1b, 0x19, 0x35);
                        visuals.panel_fill = egui::Color32::from_rgb(0x1b, 0x19, 0x35);
                        visuals.extreme_bg_color = egui::Color32::from_rgb(0x13, 0x12, 0x27);
                        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(0x24, 0x22, 0x49);
                        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x2b, 0x2b, 0x69);
                        visuals.widgets.inactive.bg_stroke = egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                        );
                        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x43, 0x45, 0xa0);
                        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(
                            1.2,
                            egui::Color32::from_rgb(0xa5, 0xa9, 0xff),
                        );
                        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0x55, 0x50, 0xc8);
                        visuals.widgets.active.bg_stroke = egui::Stroke::new(
                            1.4,
                            egui::Color32::from_rgb(0xc0, 0xc2, 0xff),
                        );
                        visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(22);
                        visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(22);
                        visuals.widgets.active.corner_radius = egui::CornerRadius::same(22);
                        visuals.override_text_color = Some(egui::Color32::from_rgb(0xf2, 0xf1, 0xff));

                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(&name)
                                    .size(15.0)
                                    .strong()
                                    .color(egui::Color32::from_rgb(0xf2, 0xf1, 0xff)),
                            );
                        });
                        ui.add_space(8.0);
                        if !is_shapable(&key) {
                            ui.label(
                                egui::RichText::new("Unattributed traffic cannot be limited")
                                    .size(16.0)
                                    .weak()
                                    .color(egui::Color32::from_rgb(0xb8, 0xb7, 0xd7)),
                            );
                        } else {
                            if is_system(&key) {
                                ui.label(
                                    egui::RichText::new(
                                        "Kernel traffic includes VPN tunnels, network shares and Windows Update.",
                                    )
                                    .size(14.0)
                                    .weak()
                                    .color(egui::Color32::from_rgb(0xb8, 0xb7, 0xd7)),
                                );
                                ui.add_space(8.0);
                            }
                            let button_width = ui.available_width();
                            let action_button = |asset: &'static str, bytes: &'static [u8], label: String| {
                                egui::Button::image_and_text(
                                    egui::Image::from_bytes(asset, bytes)
                                        .fit_to_exact_size(egui::vec2(24.0, 24.0)),
                                    egui::RichText::new(label)
                                        .size(16.0)
                                        .strong()
                                        .color(egui::Color32::from_rgb(0xf2, 0xf1, 0xff)),
                                )
                                .fill(egui::Color32::from_rgb(0x2b, 0x2b, 0x69))
                                .stroke(egui::Stroke::new(
                                    1.0,
                                    egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                                ))
                                .corner_radius(egui::CornerRadius::same(22))
                            };
                            if ui.add_sized(
                                [button_width, 50.0],
                                action_button(
                                    "bytes://netflux-limit-action.svg",
                                    include_bytes!("../assets/TrafficRIcon.svg"),
                                    "SET BANDWIDTH LIMITS…".to_string(),
                                ),
                            ).clicked() {
                                self.open_limit_modal(&key, &name);
                                self.process_action_window = None;
                            }
                            ui.add_space(10.0);
                            let blocked = self.last_snapshot.rules.get(&key).is_some_and(|r| r.blocked);
                            if ui.add_sized(
                                [button_width, 50.0],
                                action_button(
                                    "bytes://netflux-block-action.svg",
                                    include_bytes!("../assets/BlockedIcon.svg"),
                                    if blocked { "UNBLOCK PROCESS" } else { "BLOCK PROCESS" }.to_string(),
                                ),
                            ).clicked() {
                                self.set_blocked(&key, !blocked);
                                self.process_action_window = None;
                            }
                            ui.add_space(10.0);
                            if ui.add_sized(
                                [button_width, 50.0],
                                action_button(
                                    "bytes://netflux-remove-action.svg",
                                    include_bytes!("../assets/AllowedIcon.svg"),
                                    "REMOVE RULE".to_string(),
                                ),
                            ).clicked() {
                                let _ = self.cmd_tx.send(Command::RemoveRule(key.clone()));
                                self.process_action_window = None;
                            }
                        }
                        ui.add_space(4.0);
                    });
            });
        if !open || close_clicked {
            self.process_action_window = None;
        }
    }

    // Edit both independent machine-wide ceilings. Empty values mean unlimited.
    fn draw_global_limit_modal(&mut self, ctx: &egui::Context) {
        let Some(mut modal) = self.global_limit_modal.take() else {
            return;
        };

        let mut open = true;
        let mut close_clicked = false;
        let mut apply = false;
        let mut cancel = false;

        egui::Window::new("GLOBAL BANDWIDTH LIMITS")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x12, 0x11, 0x16))
                    .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                    .corner_radius(egui::CornerRadius::same(13))
                    .inner_margin(0.0),
            )
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(650.0)
            .min_width(650.0)
            .min_height(310.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                draw_dialog_header(
                    ui,
                    "GLOBAL BANDWIDTH LIMITS",
                    "global-bandwidth-dialog-close",
                    &mut close_clicked,
                    11,
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        {
                            let visuals = ui.visuals_mut();
                            visuals.window_fill = egui::Color32::from_rgb(0x12, 0x11, 0x16);
                            visuals.extreme_bg_color = egui::Color32::from_rgb(0x0d, 0x0d, 0x10);
                            visuals.widgets.inactive.bg_fill =
                                egui::Color32::from_rgb(0x17, 0x1a, 0x24);
                            visuals.widgets.inactive.bg_stroke =
                                egui::Stroke::new(1.0, COLOR_BORDER);
                            visuals.widgets.hovered.bg_fill =
                                egui::Color32::from_rgb(0x25, 0x35, 0x4d);
                            visuals.widgets.active.bg_fill =
                                egui::Color32::from_rgb(0x2b, 0x43, 0x69);
                        }

                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(
                                    "Set independent maximum speeds for the entire computer",
                                )
                                .size(14.0)
                                .color(COLOR_MUTED),
                            );
                        });
                        ui.add_space(20.0);

                        ui.columns(2, |columns| {
                            for (index, ui) in columns.iter_mut().enumerate() {
                                let (title, color, value, unit, unit_id) = if index == 0 {
                                    (
                                        "MAX DOWNLOAD",
                                        COLOR_DOWN,
                                        &mut modal.down_value,
                                        &mut modal.down_unit,
                                        "global-download-unit",
                                    )
                                } else {
                                    (
                                        "MAX UPLOAD",
                                        COLOR_UP,
                                        &mut modal.up_value,
                                        &mut modal.up_unit,
                                        "global-upload-unit",
                                    )
                                };

                                egui::Frame::new()
                                    .fill(egui::Color32::from_rgb(0x17, 0x1a, 0x24))
                                    .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                                    .corner_radius(egui::CornerRadius::same(13))
                                    .inner_margin(egui::Margin::same(16))
                                    .show(ui, |ui| {
                                        ui.vertical_centered(|ui| {
                                            ui.label(
                                                egui::RichText::new(title)
                                                    .size(15.0)
                                                    .strong()
                                                    .color(color),
                                            );
                                            ui.add_space(12.0);
                                            ui.horizontal(|ui| {
                                                ui.add_sized(
                                                    [175.0, 42.0],
                                                    egui::TextEdit::singleline(value)
                                                        .font(egui::TextStyle::Heading)
                                                        .horizontal_align(egui::Align::Center)
                                                        .hint_text("Unlimited")
                                                        .vertical_align(egui::Align::Center),
                                                );
                                                egui::ComboBox::from_id_salt(unit_id)
                                                    .selected_text(unit.label())
                                                    .width(72.0)
                                                    .show_ui(ui, |ui| {
                                                        for rate_unit in RateUnit::ALL {
                                                            ui.selectable_value(
                                                                unit,
                                                                rate_unit,
                                                                rate_unit.label(),
                                                            );
                                                        }
                                                    });
                                            });
                                        });
                                    });
                            }
                        });

                        if let Some(error) = &modal.error {
                            ui.add_space(10.0);
                            ui.colored_label(COLOR_BLOCK, error);
                        }

                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Leave a field empty for unlimited.")
                                    .size(12.0)
                                    .color(COLOR_MUTED),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_sized(
                                            [108.0, 38.0],
                                            egui::Button::new("Cancel")
                                                .fill(egui::Color32::from_rgb(0x2b, 0x2b, 0x69))
                                                .stroke(egui::Stroke::new(
                                                    1.0,
                                                    egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                                                ))
                                                .corner_radius(egui::CornerRadius::same(18)),
                                        )
                                        .clicked()
                                    {
                                        cancel = true;
                                    }
                                    if ui
                                        .add_sized(
                                            [108.0, 38.0],
                                            egui::Button::new(
                                                egui::RichText::new("Apply").strong(),
                                            )
                                            .fill(egui::Color32::from_rgb(0x55, 0x50, 0xc8))
                                            .stroke(egui::Stroke::new(
                                                1.2,
                                                egui::Color32::from_rgb(0xc0, 0xc2, 0xff),
                                            ))
                                            .corner_radius(egui::CornerRadius::same(18)),
                                        )
                                        .clicked()
                                    {
                                        apply = true;
                                    }
                                },
                            );
                        });
                    });
            });

        if cancel || !open || close_clicked {
            return;
        }

        if apply {
            let parse_limit = |value: &str, unit: RateUnit| -> Result<Option<u64>, ()> {
                let trimmed = value.trim().replace(',', ".");
                if trimmed.is_empty() {
                    return Ok(None);
                }
                match trimmed.parse::<f64>() {
                    Ok(value) if value.is_finite() && value >= 0.0 => {
                        Ok(Some((value * unit.multiplier()) as u64))
                    }
                    _ => Err(()),
                }
            };

            match (
                parse_limit(&modal.down_value, modal.down_unit),
                parse_limit(&modal.up_value, modal.up_unit),
            ) {
                (Ok(down_limit), Ok(up_limit)) => {
                    self.send_global_limits(GlobalLimits {
                        down_limit,
                        up_limit,
                    });
                    return;
                }
                _ => {
                    modal.error =
                        Some("Enter a valid non-negative number for each limit.".to_string());
                }
            }
        }

        self.global_limit_modal = Some(modal);
    }

    // The limit editor uses the same custom in-frame header as the other dialogs.
    fn draw_limit_modal(&mut self, ctx: &egui::Context) {
        let Some(mut modal) = self.limit_modal.take() else {
            return;
        };
        let presets = self.presets.clone();

        let mut open = true;
        let mut close_clicked = false;
        let mut apply = false;
        let mut cancel = false;

        egui::Window::new("SET BANDWIDTH LIMITS")
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(0x12, 0x11, 0x16))
                    .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                    .corner_radius(egui::CornerRadius::same(13))
                    .inner_margin(0.0),
            )
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(800.0)
            .min_width(800.0)
            .min_height(500.0)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                draw_dialog_header(
                    ui,
                    "SET BANDWIDTH LIMITS",
                    "bandwidth-limits-dialog-close",
                    &mut close_clicked,
                    11,
                );
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(18))
                    .show(ui, |ui| {
                        // Keep every control in this dialog on the NetFlux dark palette.
                        {
                            let visuals = ui.visuals_mut();
                            visuals.window_fill = egui::Color32::from_rgb(0x12, 0x11, 0x16);
                            visuals.extreme_bg_color = egui::Color32::from_rgb(0x0d, 0x0d, 0x10);
                            visuals.widgets.noninteractive.bg_fill =
                                egui::Color32::from_rgb(0x12, 0x11, 0x16);
                            visuals.widgets.inactive.bg_fill =
                                egui::Color32::from_rgb(0x17, 0x1a, 0x24);
                            visuals.widgets.inactive.bg_stroke =
                                egui::Stroke::new(1.0, COLOR_BORDER);
                            visuals.widgets.hovered.bg_fill =
                                egui::Color32::from_rgb(0x25, 0x35, 0x4d);
                            visuals.widgets.hovered.bg_stroke =
                                egui::Stroke::new(1.0, COLOR_ACCENT);
                            visuals.widgets.active.bg_fill =
                                egui::Color32::from_rgb(0x2b, 0x43, 0x69);
                            visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, COLOR_DOWN);
                        }
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new(&modal.name).strong().size(17.0));
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("Set independent download and upload speeds")
                                    .size(13.0)
                                    .color(COLOR_MUTED),
                            );
                        });
                        ui.add_space(18.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new("SAVED PRESETS")
                                    .size(10.0)
                                    .strong()
                                    .color(COLOR_MUTED),
                            );
                            ui.add_space(8.0);
                            for preset in &presets {
                                let response = ui.add_sized(
                                    [130.0, 36.0],
                                    egui::Button::new(
                                        egui::RichText::new(&preset.name).size(12.0).strong(),
                                    )
                                    .fill(egui::Color32::from_rgb(0x2b, 0x2b, 0x69))
                                    .stroke(egui::Stroke::new(
                                        1.0,
                                        egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                                    ))
                                    .corner_radius(egui::CornerRadius::same(18)),
                                );
                                if response.clicked() {
                                    modal.down_value = preset_rate_value(preset.down_limit);
                                    modal.down_unit = RateUnit::MBps;
                                    modal.up_value = preset_rate_value(preset.up_limit);
                                    modal.up_unit = RateUnit::MBps;
                                    modal.error = None;
                                }
                                response.on_hover_text(format!(
                                    "Download: {} · Upload: {}",
                                    preset_rate_label(preset.down_limit),
                                    preset_rate_label(preset.up_limit),
                                ));
                            }
                        });
                        ui.add_space(18.0);

                        ui.columns(2, |columns| {
                            for (index, ui) in columns.iter_mut().enumerate() {
                                let (title, color, value, unit, is_download) = if index == 0 {
                                    (
                                        "DOWNLOAD LIMIT",
                                        COLOR_DOWN,
                                        &mut modal.down_value,
                                        &mut modal.down_unit,
                                        true,
                                    )
                                } else {
                                    (
                                        "UPLOAD LIMIT",
                                        COLOR_UP,
                                        &mut modal.up_value,
                                        &mut modal.up_unit,
                                        false,
                                    )
                                };

                                egui::Frame::new()
                                    .fill(egui::Color32::from_rgb(0x17, 0x1a, 0x24))
                                    .stroke(egui::Stroke::new(1.0, COLOR_BORDER))
                                    .corner_radius(egui::CornerRadius::same(13))
                                    .inner_margin(egui::Margin::same(20))
                                    .show(ui, |ui| {
                                        ui.set_min_height(218.0);
                                        ui.vertical_centered(|ui| {
                                            draw_direction_icon(ui, color, is_download);
                                            ui.add_space(12.0);
                                            ui.label(
                                                egui::RichText::new(title)
                                                    .strong()
                                                    .size(17.0)
                                                    .color(color),
                                            );
                                            ui.add_space(16.0);

                                            ui.horizontal(|ui| {
                                                let controls_width =
                                                    190.0 + 76.0 + ui.spacing().item_spacing.x;
                                                ui.add_space(
                                                    ((ui.available_width() - controls_width) / 2.0)
                                                        .max(0.0),
                                                );
                                                ui.add_sized(
                                                    [190.0, 46.0],
                                                    egui::TextEdit::singleline(value)
                                                        .font(egui::TextStyle::Heading)
                                                        .horizontal_align(egui::Align::Center)
                                                        .hint_text("Unlimited")
                                                        .vertical_align(egui::Align::Center),
                                                );
                                                ui.scope(|ui| {
                                                    // ComboBox paints its own button; style that widget
                                                    // directly instead of adding a padded frame around it.
                                                    let visuals = &mut ui.visuals_mut().widgets;
                                                    visuals.noninteractive.bg_fill =
                                                        egui::Color32::from_rgb(0x17, 0x1a, 0x24);
                                                    visuals.inactive.bg_fill =
                                                        egui::Color32::from_rgb(0x17, 0x1a, 0x24);
                                                    visuals.hovered.bg_fill =
                                                        egui::Color32::from_rgb(0x25, 0x35, 0x4d);
                                                    visuals.active.bg_fill =
                                                        egui::Color32::from_rgb(0x2b, 0x43, 0x69);

                                                    egui::ComboBox::from_id_salt(if is_download {
                                                        "download-rate-unit"
                                                    } else {
                                                        "upload-rate-unit"
                                                    })
                                                    .selected_text(unit.label())
                                                    .width(76.0)
                                                    .show_ui(ui, |ui| {
                                                        for rate_unit in RateUnit::ALL {
                                                            ui.selectable_value(
                                                                unit,
                                                                rate_unit,
                                                                rate_unit.label(),
                                                            );
                                                        }
                                                    });
                                                });
                                            });
                                        });

                                        ui.vertical_centered(|ui| {
                                            ui.add_space(14.0);
                                            ui.label(
                                                egui::RichText::new("QUICK PRESETS")
                                                    .size(10.0)
                                                    .strong()
                                                    .color(COLOR_MUTED),
                                            );
                                            ui.add_space(6.0);
                                            let quick_values: [(&str, &str); 2] = if is_download {
                                                [("1 MB/s", "1"), ("5 MB/s", "5")]
                                            } else {
                                                [("0.25 MB/s", "0.25"), ("1 MB/s", "1")]
                                            };
                                            let (button_fill, button_stroke) = if is_download {
                                                (
                                                    egui::Color32::from_rgb(0x18, 0x2a, 0x40),
                                                    COLOR_DOWN,
                                                )
                                            } else {
                                                (
                                                    egui::Color32::from_rgb(0x3a, 0x22, 0x1e),
                                                    COLOR_UP,
                                                )
                                            };
                                            ui.horizontal(|ui| {
                                                let buttons_width =
                                                    145.0 * 2.0 + ui.spacing().item_spacing.x;
                                                ui.add_space(
                                                    ((ui.available_width() - buttons_width) / 2.0)
                                                        .max(0.0),
                                                );
                                                for (label, preset_value) in quick_values {
                                                    if ui
                                                        .add_sized(
                                                            [145.0, 32.0],
                                                            egui::Button::new(
                                                                egui::RichText::new(label)
                                                                    .size(12.0)
                                                                    .strong(),
                                                            )
                                                            .fill(button_fill)
                                                            .stroke(egui::Stroke::new(
                                                                1.0,
                                                                button_stroke,
                                                            ))
                                                            .corner_radius(
                                                                egui::CornerRadius::same(16),
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        *value = preset_value.to_string();
                                                        *unit = RateUnit::MBps;
                                                        modal.error = None;
                                                    }
                                                }
                                            });
                                        });
                                    });
                            }
                        });

                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("LIMIT DURATION")
                                    .size(11.0)
                                    .strong()
                                    .color(COLOR_MUTED),
                            );
                            ui.add_space(10.0);
                            egui::ComboBox::from_id_salt("limit-duration")
                                .selected_text(match modal.duration {
                                    LimitDuration::Permanent => "Permanent",
                                    LimitDuration::ThirtyMinutes => "30 minutes",
                                    LimitDuration::OneHour => "1 hour",
                                    LimitDuration::TwoHours => "2 hours",
                                    LimitDuration::Custom => "Custom",
                                    LimitDuration::PreserveExisting => "Keep existing",
                                })
                                .width(180.0)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut modal.duration,
                                        LimitDuration::Permanent,
                                        "Permanent",
                                    );
                                    ui.selectable_value(
                                        &mut modal.duration,
                                        LimitDuration::ThirtyMinutes,
                                        "30 minutes",
                                    );
                                    ui.selectable_value(
                                        &mut modal.duration,
                                        LimitDuration::OneHour,
                                        "1 hour",
                                    );
                                    ui.selectable_value(
                                        &mut modal.duration,
                                        LimitDuration::TwoHours,
                                        "2 hours",
                                    );
                                    ui.selectable_value(
                                        &mut modal.duration,
                                        LimitDuration::Custom,
                                        "Custom",
                                    );
                                    if modal.exe_paths.len() > 1 {
                                        ui.selectable_value(
                                            &mut modal.duration,
                                            LimitDuration::PreserveExisting,
                                            "Keep existing durations",
                                        );
                                    }
                                });
                        });

                        if modal.duration == LimitDuration::Custom {
                            ui.add_space(9.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("CUSTOM DURATION")
                                        .size(11.0)
                                        .strong()
                                        .color(COLOR_MUTED),
                                );
                                ui.add_space(10.0);
                                ui.add_sized(
                                    [90.0, 34.0],
                                    egui::TextEdit::singleline(&mut modal.custom_hours)
                                        .hint_text("0")
                                        .horizontal_align(egui::Align::Center)
                                        .vertical_align(egui::Align::Center),
                                );
                                ui.label("Hours");
                                ui.add_space(12.0);
                                ui.add_sized(
                                    [90.0, 34.0],
                                    egui::TextEdit::singleline(&mut modal.custom_minutes)
                                        .hint_text("0")
                                        .horizontal_align(egui::Align::Center)
                                        .vertical_align(egui::Align::Center),
                                );
                                ui.label("Minutes");
                                ui.label(
                                    egui::RichText::new("Blank = 0")
                                        .size(11.0)
                                        .color(COLOR_MUTED),
                                );
                            });
                        }

                        if let Some(err) = &modal.error {
                            ui.add_space(10.0);
                            ui.colored_label(COLOR_BLOCK, err);
                        }
                        ui.add_space(22.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Leave a speed field empty for unlimited.")
                                    .size(12.0)
                                    .color(COLOR_MUTED),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_sized(
                                            [112.0, 40.0],
                                            egui::Button::new("Cancel")
                                                .fill(egui::Color32::from_rgb(0x2b, 0x2b, 0x69))
                                                .stroke(egui::Stroke::new(
                                                    1.0,
                                                    egui::Color32::from_rgb(0x65, 0x6c, 0xd9),
                                                ))
                                                .corner_radius(egui::CornerRadius::same(18)),
                                        )
                                        .clicked()
                                    {
                                        cancel = true;
                                    }
                                    if ui
                                        .add_sized(
                                            [112.0, 40.0],
                                            egui::Button::new(
                                                egui::RichText::new("Apply").strong(),
                                            )
                                            .fill(egui::Color32::from_rgb(0x55, 0x50, 0xc8))
                                            .stroke(egui::Stroke::new(
                                                1.2,
                                                egui::Color32::from_rgb(0xc0, 0xc2, 0xff),
                                            ))
                                            .corner_radius(egui::CornerRadius::same(18)),
                                        )
                                        .clicked()
                                    {
                                        apply = true;
                                    }
                                },
                            );
                        });
                        ui.add_space(5.0);
                    });
            });

        if cancel || !open || close_clicked {
            return;
        }

        if apply {
            let parse_limit = |value: &str, unit: RateUnit| -> Result<Option<u64>, ()> {
                let trimmed = value.trim().replace(',', ".");
                if trimmed.is_empty() {
                    return Ok(None);
                }
                match trimmed.parse::<f64>() {
                    Ok(v) if v.is_finite() && v >= 0.0 => Ok(Some((v * unit.multiplier()) as u64)),
                    _ => Err(()),
                }
            };

            let preserve_duration = modal.duration == LimitDuration::PreserveExisting;
            let duration_seconds = if preserve_duration {
                Ok(None)
            } else {
                match modal.duration {
                    LimitDuration::Permanent => Ok(None),
                    LimitDuration::ThirtyMinutes => Ok(Some(30 * 60)),
                    LimitDuration::OneHour => Ok(Some(60 * 60)),
                    LimitDuration::TwoHours => Ok(Some(2 * 60 * 60)),
                    LimitDuration::Custom => {
                        let parse_component = |value: &str| -> Result<u64, ()> {
                            let trimmed = value.trim();
                            if trimmed.is_empty() {
                                Ok(0)
                            } else {
                                trimmed.parse::<u64>().map_err(|_| ())
                            }
                        };
                        match (
                            parse_component(&modal.custom_hours),
                            parse_component(&modal.custom_minutes),
                        ) {
                            (Ok(hours), Ok(minutes)) => hours
                                .checked_mul(3600)
                                .and_then(|hours| {
                                    minutes
                                        .checked_mul(60)
                                        .and_then(|minutes| hours.checked_add(minutes))
                                })
                                .filter(|seconds| *seconds > 0)
                                .map(Some)
                                .ok_or(()),
                            _ => Err(()),
                        }
                    }
                    LimitDuration::PreserveExisting => unreachable!(),
                }
            };

            let down_limit = parse_limit(&modal.down_value, modal.down_unit);
            let up_limit = parse_limit(&modal.up_value, modal.up_unit);

            match (down_limit, up_limit, duration_seconds) {
                (Ok(down), Ok(up), Ok(duration)) => {
                    let expires_at = match duration {
                        Some(seconds) => match unix_time_secs().checked_add(seconds) {
                            Some(expires_at) => Some(expires_at),
                            None => {
                                modal.error =
                                    Some("The selected duration is too long.".to_string());
                                self.limit_modal = Some(modal);
                                return;
                            }
                        },
                        None => None,
                    };

                    for path in &modal.exe_paths {
                        let mut rule = self.base_rule(path);
                        rule.exe_path = path.clone();
                        rule.down_limit = down;
                        rule.up_limit = up;
                        if !preserve_duration {
                            rule.expires_at = expires_at;
                            if expires_at.is_none() {
                                rule.restore = None;
                            }
                        }
                        self.send_rule(rule);
                    }
                    return;
                }
                (Err(_), _, _) | (_, Err(_), _) => {
                    modal.error =
                        Some("Enter a valid non-negative number for each limit.".to_string());
                }
                (_, _, Err(_)) => {
                    modal.error =
                        Some("Enter a valid custom duration of at least 1 minute.".to_string());
                }
            }
        }

        self.limit_modal = Some(modal);
    }
}

impl eframe::App for NetFluxApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Re-assert the app palette every frame so OS appearance changes cannot
        // switch egui widgets back to the Windows light palette.
        apply_dark_theme(&ctx);

        // Drain any pending snapshots each frame.
        let incoming: Vec<Snapshot> = self.snapshot_rx.try_iter().collect();
        for snap in incoming {
            self.total_down_hist.push(snap.total_down_rate as f64);
            self.total_up_hist.push(snap.total_up_rate as f64);
            if self.total_down_hist.len() > TOTAL_HISTORY_LEN {
                let excess = self.total_down_hist.len() - TOTAL_HISTORY_LEN;
                self.total_down_hist.drain(0..excess);
            }
            if self.total_up_hist.len() > TOTAL_HISTORY_LEN {
                let excess = self.total_up_hist.len() - TOTAL_HISTORY_LEN;
                self.total_up_hist.drain(0..excess);
            }
            if self.pending_global_limits == Some(snap.global_limits) {
                self.pending_global_limits = None;
            }
            self.last_snapshot = snap;
        }

        // Closing can hide the window in the system tray without stopping the backend.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.close_to_tray && !self.force_exit {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                self.send_shutdown();
            }
        }

        let snap = self.last_snapshot.clone();
        self.process_icons
            .retain(|key, _| snap.processes.contains_key(key));
        self.selected_processes
            .retain(|key| snap.processes.contains_key(key));
        if self
            .selection_anchor
            .as_ref()
            .is_some_and(|key| !snap.processes.contains_key(key))
        {
            self.selection_anchor = None;
        }

        // Clear a stale selection so the side panel doesn't linger.
        if let Some(sel) = &self.selected
            && !snap.processes.contains_key(sel)
        {
            self.selected = None;
        }

        if let Ok(update) = self.update_rx.try_recv() {
            self.update = Some(update);
        }

        // Paint the full app area, including the 40 px side gutters, with the
        // dashboard background color.
        ui.painter().rect_filled(
            ui.max_rect(),
            0.0,
            egui::Color32::from_rgb(0x12, 0x11, 0x16),
        );

        let content_rect = ui.max_rect().shrink2(egui::vec2(39.0, 0.0));

        ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
            ui.set_width(content_rect.width());

            self.draw_menu_bar(ui);
            self.draw_update_banner(ui);
            self.draw_top_panel(ui, &snap);
            self.draw_process_toolbar(ui, &snap);
            self.draw_table(ui, &snap);
            self.draw_process_action_window(&ctx);
            self.draw_global_limit_modal(&ctx);
            self.draw_limit_modal(&ctx);
            self.draw_preset_manager(&ctx);
            self.draw_preset_editor(&ctx);
            self.draw_about(&ctx);
        });

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl Drop for NetFluxApp {
    fn drop(&mut self) {
        self.send_shutdown();
    }
}

#[cfg(windows)]
fn extract_process_icon(path: &str) -> Option<egui::ColorImage> {
    use std::mem::size_of;
    use std::ptr::null_mut;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
        RGBQUAD, ReleaseDC,
    };
    use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    let mut wide: Vec<u16> = path.encode_utf16().collect();
    wide.push(0);
    let mut file_info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let found = unsafe {
        SHGetFileInfoW(
            wide.as_ptr(),
            0,
            &mut file_info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if found == 0 || file_info.hIcon.is_null() {
        return None;
    }

    let mut icon_info: ICONINFO = unsafe { std::mem::zeroed() };
    let got_icon = unsafe { GetIconInfo(file_info.hIcon, &mut icon_info) } != 0;
    let mut result = None;
    if got_icon && !icon_info.hbmColor.is_null() {
        let hdc = unsafe { GetDC(null_mut()) };
        if !hdc.is_null() {
            let mut pixels = vec![0u8; 32 * 32 * 4];
            let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: 32,
                biHeight: -32,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..unsafe { std::mem::zeroed() }
            };
            bmi.bmiColors[0] = RGBQUAD {
                rgbBlue: 0,
                rgbGreen: 0,
                rgbRed: 0,
                rgbReserved: 0,
            };
            let lines = unsafe {
                GetDIBits(
                    hdc,
                    icon_info.hbmColor,
                    0,
                    32,
                    pixels.as_mut_ptr().cast(),
                    &mut bmi,
                    DIB_RGB_COLORS,
                )
            };
            unsafe { ReleaseDC(null_mut(), hdc) };
            if lines > 0 {
                for px in pixels.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                    if px[3] == 0 {
                        px[3] = 255;
                    }
                }
                result = Some(egui::ColorImage::from_rgba_unmultiplied([32, 32], &pixels));
            }
        }
    }
    unsafe {
        if !icon_info.hbmColor.is_null() {
            DeleteObject(icon_info.hbmColor);
        }
        if !icon_info.hbmMask.is_null() {
            DeleteObject(icon_info.hbmMask);
        }
        DestroyIcon(file_info.hIcon);
    }
    result
}

#[cfg(not(windows))]
fn extract_process_icon(_path: &str) -> Option<egui::ColorImage> {
    None
}

// ---------------------------------------------------------------------------
// Shell helpers
// ---------------------------------------------------------------------------

/// Open a URL in the default browser.
fn open_url(ctx: &egui::Context, url: &str) {
    ctx.open_url(egui::OpenUrl::new_tab(url));
}

/// Open a file with its default application, or a folder in Explorer.
/// Best-effort: failures are logged, never surfaced as errors.
fn open_path(path: &std::path::Path) {
    if let Err(e) = std::process::Command::new("explorer").arg(path).spawn() {
        tracing::warn!("failed to open {}: {e}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Fallback error window (shown when the backend can't start, e.g. not elevated
// or WinDivert is missing).
// ---------------------------------------------------------------------------

pub struct ErrorApp {
    message: String,
}

impl ErrorApp {
    pub fn new(message: String) -> Self {
        Self { message }
    }
}

impl eframe::App for ErrorApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.heading("NetFlux could not start");
            });
            ui.add_space(12.0);
            ui.label(
                "The backend failed to initialize. This usually means the app is not \
                 running as Administrator, or the WinDivert driver files \
                 (WinDivert.dll / WinDivert64.sys) are missing next to the executable.",
            );
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(egui::RichText::new("Details:").strong());
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.label(
                    egui::RichText::new(&self.message)
                        .monospace()
                        .color(COLOR_BLOCK),
                );
            });
            ui.add_space(12.0);
            ui.separator();
            if ui.button("Close").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}
