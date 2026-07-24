//! Pure domain rules for the Windows Codex monitor.
//!
//! The application intentionally presents remaining weekly usage. Codex reports
//! used usage, so the conversion belongs here rather than in the tray UI.

use serde::Deserialize;
use thiserror::Error;

pub const WEEKLY_WINDOW_MINUTES: u64 = 10_080;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeeklyQuota {
    pub remaining_percent: u8,
    pub resets_at_unix_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GaugeZone {
    Green,
    Yellow,
    Orange,
    Red,
}

impl GaugeZone {
    pub fn for_remaining(remaining_percent: u8) -> Self {
        match remaining_percent {
            50..=100 => Self::Green,
            25..=49 => Self::Yellow,
            10..=24 => Self::Orange,
            _ => Self::Red,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatus {
    Working,
    Waiting,
    Idle,
    Offline,
    SuspectedHung,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSignal {
    Completed,
    Waiting,
    Activity,
}

pub fn classify_monitor_status(
    codex_process_present: bool,
    signal: SessionSignal,
    seconds_since_activity: u64,
) -> MonitorStatus {
    if !codex_process_present {
        return MonitorStatus::Offline;
    }
    match signal {
        SessionSignal::Completed => MonitorStatus::Idle,
        SessionSignal::Waiting => MonitorStatus::Waiting,
        SessionSignal::Activity if seconds_since_activity <= 90 => MonitorStatus::Working,
        SessionSignal::Activity if seconds_since_activity <= 600 => MonitorStatus::SuspectedHung,
        SessionSignal::Activity => MonitorStatus::Idle,
    }
}

impl MonitorStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Idle => "Idle",
            Self::Offline => "Offline",
            Self::SuspectedHung => "Suspected Hung",
        }
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum QuotaError {
    #[error("Codex returned invalid JSON: {0}")]
    InvalidJson(String),
    #[error("Codex did not return a primary rate-limit window")]
    MissingPrimaryWindow,
    #[error("Codex returned an unsupported rate-limit window: {0} minutes")]
    UnsupportedWindowDuration(u64),
    #[error("Codex returned a non-finite used percentage")]
    NonFiniteUsedPercent,
}

#[derive(Debug, Deserialize)]
struct RateLimitResponse {
    result: RateLimitResult,
}

#[derive(Debug, Deserialize)]
struct RateLimitResult {
    #[serde(rename = "rateLimits")]
    rate_limits: RateLimits,
}

#[derive(Debug, Deserialize)]
struct RateLimits {
    primary: Option<RateLimitWindow>,
}

#[derive(Debug, Deserialize)]
struct RateLimitWindow {
    #[serde(rename = "usedPercent")]
    used_percent: f64,
    #[serde(rename = "windowDurationMins")]
    window_duration_mins: Option<u64>,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
}

pub fn parse_weekly_quota(json: &str) -> Result<WeeklyQuota, QuotaError> {
    let response: RateLimitResponse =
        serde_json::from_str(json).map_err(|error| QuotaError::InvalidJson(error.to_string()))?;
    let primary = response
        .result
        .rate_limits
        .primary
        .ok_or(QuotaError::MissingPrimaryWindow)?;
    let duration = primary
        .window_duration_mins
        .ok_or(QuotaError::MissingPrimaryWindow)?;

    if duration != WEEKLY_WINDOW_MINUTES {
        return Err(QuotaError::UnsupportedWindowDuration(duration));
    }
    if !primary.used_percent.is_finite() {
        return Err(QuotaError::NonFiniteUsedPercent);
    }

    let remaining = (100.0 - primary.used_percent).round().clamp(0.0, 100.0) as u8;
    Ok(WeeklyQuota {
        remaining_percent: remaining,
        resets_at_unix_seconds: primary.resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(used_percent: &str, duration: u64) -> String {
        format!(
            r#"{{"result":{{"rateLimits":{{"primary":{{"usedPercent":{used_percent},"windowDurationMins":{duration},"resetsAt":1785258188}}}}}}}}"#
        )
    }

    #[test]
    fn converts_used_percent_to_remaining_weekly_percent() {
        let quota = parse_weekly_quota(&response("64", WEEKLY_WINDOW_MINUTES)).unwrap();
        assert_eq!(quota.remaining_percent, 36);
        assert_eq!(quota.resets_at_unix_seconds, Some(1_785_258_188));
    }

    #[test]
    fn rounds_and_clamps_remaining_percent() {
        assert_eq!(
            parse_weekly_quota(&response("64.6", WEEKLY_WINDOW_MINUTES))
                .unwrap()
                .remaining_percent,
            35
        );
        assert_eq!(
            parse_weekly_quota(&response("-20", WEEKLY_WINDOW_MINUTES))
                .unwrap()
                .remaining_percent,
            100
        );
        assert_eq!(
            parse_weekly_quota(&response("120", WEEKLY_WINDOW_MINUTES))
                .unwrap()
                .remaining_percent,
            0
        );
    }

    #[test]
    fn assigns_every_gauge_zone_at_the_boundary() {
        assert_eq!(GaugeZone::for_remaining(100), GaugeZone::Green);
        assert_eq!(GaugeZone::for_remaining(50), GaugeZone::Green);
        assert_eq!(GaugeZone::for_remaining(49), GaugeZone::Yellow);
        assert_eq!(GaugeZone::for_remaining(25), GaugeZone::Yellow);
        assert_eq!(GaugeZone::for_remaining(24), GaugeZone::Orange);
        assert_eq!(GaugeZone::for_remaining(10), GaugeZone::Orange);
        assert_eq!(GaugeZone::for_remaining(9), GaugeZone::Red);
        assert_eq!(GaugeZone::for_remaining(0), GaugeZone::Red);
    }

    #[test]
    fn rejects_missing_primary_window() {
        let result = parse_weekly_quota(r#"{"result":{"rateLimits":{"primary":null}}}"#);
        assert_eq!(result, Err(QuotaError::MissingPrimaryWindow));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(parse_weekly_quota("not json"), Err(QuotaError::InvalidJson(_))));
    }

    #[test]
    fn rejects_non_weekly_window() {
        assert_eq!(
            parse_weekly_quota(&response("10", 300)),
            Err(QuotaError::UnsupportedWindowDuration(300))
        );
    }

    #[test]
    fn classifies_watchdog_states_without_claiming_old_work_is_hung() {
        assert_eq!(
            classify_monitor_status(false, SessionSignal::Activity, 0),
            MonitorStatus::Offline
        );
        assert_eq!(
            classify_monitor_status(true, SessionSignal::Completed, 0),
            MonitorStatus::Idle
        );
        assert_eq!(
            classify_monitor_status(true, SessionSignal::Waiting, 0),
            MonitorStatus::Waiting
        );
        assert_eq!(
            classify_monitor_status(true, SessionSignal::Activity, 90),
            MonitorStatus::Working
        );
        assert_eq!(
            classify_monitor_status(true, SessionSignal::Activity, 91),
            MonitorStatus::SuspectedHung
        );
        assert_eq!(
            classify_monitor_status(true, SessionSignal::Activity, 600),
            MonitorStatus::SuspectedHung
        );
        assert_eq!(
            classify_monitor_status(true, SessionSignal::Activity, 601),
            MonitorStatus::Idle
        );
    }
}
