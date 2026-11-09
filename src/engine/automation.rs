//! Automation bridge: schedule parsing + the host-side store trait the Cron
//! tools call through. channel owns the semantics (ZCode CronCreate
//! contract); the host owns persistence and dispatch.

use serde_json::Value;

/// How an automation fires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScheduleSpec {
    /// 5-field cron (minute hour dom month dow) in the user's local
    /// timezone, plus a legal display expression for UI purposes.
    Cron { expression: String },
    /// One-shot relative delay in whole minutes from creation time.
    Delay { minutes: u32 },
    /// Every-N recurrence: unit + 1..=200 step (cron-style display fields
    /// are the host's concern; the interval rule is authoritative).
    Interval { unit: IntervalUnit, step: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalUnit {
    Minute,
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

impl IntervalUnit {
    /// Parses the wire name (`minute`..`yearly`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "minute" => Some(Self::Minute),
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            "monthly" => Some(Self::Monthly),
            "yearly" => Some(Self::Yearly),
            _ => None,
        }
    }
}

impl std::str::FromStr for IntervalUnit {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| format!("无效的间隔单位: {value}"))
    }
}

impl std::fmt::Display for IntervalUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Minute => "minute",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Yearly => "yearly",
        };
        f.write_str(name)
    }
}

impl ScheduleSpec {
    /// Builds the schedule from CronCreate tool arguments. Exactly one form
    /// must be present: `cron`, `delayMinutes`, or `intervalUnit`+`interval`.
    pub fn from_tool_args(args: &Value) -> Result<Self, String> {
        let delay = args.get("delayMinutes").and_then(Value::as_u64);
        let interval_unit = args
            .get("intervalUnit")
            .and_then(Value::as_str)
            .and_then(IntervalUnit::parse);
        let interval = args.get("interval").and_then(Value::as_u64);
        let cron = args.get("cron").and_then(Value::as_str);
        match (cron, delay, interval_unit.zip(interval)) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) | (_, Some(_), Some(_)) => Err(
                "只能提供一种调度形式：cron、delayMinutes 或 intervalUnit+interval".to_owned(),
            ),
            (Some(expression), None, None) => {
                validate_cron(expression)?;
                Ok(Self::Cron {
                    expression: expression.to_owned(),
                })
            }
            (None, Some(minutes), None) => {
                if minutes == 0 || minutes > 525_600 {
                    return Err("delayMinutes 须在 1..=525600".to_owned());
                }
                Ok(Self::Delay {
                    minutes: minutes as u32,
                })
            }
            (None, None, Some((unit, step))) => {
                if step == 0 || step > 200 {
                    return Err("interval 须在 1..=200".to_owned());
                }
                Ok(Self::Interval {
                    unit,
                    step: step as u32,
                })
            }
            _ => Err("缺少调度形式：需提供 cron、delayMinutes 或 intervalUnit+interval 之一".to_owned()),
        }
    }
}

/// One parsed cron field: values/domains/commas/steps (`*/n`, `a-b/n`)
/// all expand eagerly to a match set (bounded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CronField {
    Set(Vec<u32>),
}

/// Parses a 5-field cron expression into (minute, hour, dom, month, dow)
/// match sets. Names (`JAN`/`MON`) and `@`-shortcuts are not supported
/// (numbers only, matching the ZCode tool description's "standard 5-field"
/// scope).
pub(crate) fn parse_cron(expression: &str) -> Result<[CronField; 5], String> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron 表达式须为 5 个字段（分 时 日 月 周），收到 {} 个: {expression}",
            fields.len()
        ));
    }
    let parse_one = |spec: &str, min: u32, max: u32| -> Result<CronField, String> {
        let mut values = std::collections::BTreeSet::new();
        for part in spec.split(',') {
            let (range_part, step) = match part.split_once('/') {
                Some((range, step)) => {
                    let step: u32 = step
                        .parse()
                        .map_err(|_| format!("cron 步长无效: {part}"))?;
                    if step == 0 {
                        return Err(format!("cron 步长不能为 0: {part}"));
                    }
                    (range, step)
                }
                None => (part, 1),
            };
            let (start, end) = if range_part == "*" {
                (min, max)
            } else if let Some((a, b)) = range_part.split_once('-') {
                let start: u32 = a.parse().map_err(|_| format!("cron 值无效: {range_part}"))?;
                let end: u32 = b.parse().map_err(|_| format!("cron 值无效: {range_part}"))?;
                (start, end)
            } else {
                let single: u32 = range_part
                    .parse()
                    .map_err(|_| format!("cron 值无效: {range_part}"))?;
                (single, if step > 1 { max } else { single })
            };
            if start < min || end > max || start > end {
                return Err(format!(
                    "cron 字段超出范围 [{min}..{max}]: {range_part}"
                ));
            }
            let mut value = start;
            loop {
                values.insert(value);
                if value >= end {
                    break;
                }
                value += step;
                if value > max {
                    break;
                }
            }
        }
        Ok(CronField::Set(values.into_iter().collect()))
    };
    Ok([
        parse_one(fields[0], 0, 59)?,
        parse_one(fields[1], 0, 23)?,
        parse_one(fields[2], 1, 31)?,
        parse_one(fields[3], 1, 12)?,
        // dow: 0 and 7 both mean Sunday.
        parse_one(fields[4], 0, 7)?,
    ])
}

fn validate_cron(expression: &str) -> Result<(), String> {
    parse_cron(expression).map(|_| ())
}

/// Local wall-clock broken-down time (no external chrono dependency).
struct LocalTime {
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    /// 0 = Sunday ... 6 = Saturday.
    weekday: u32,
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Computes local time from a Unix timestamp in seconds using the TZ
/// environment offset captured at startup (`TZ`-less systems fall back to
/// UTC; the host process runs in the user's local timezone context).
fn local_time(unix_seconds: i64, utc_offset_seconds: i64) -> LocalTime {
    let local = unix_seconds + utc_offset_seconds;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (_year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday (weekday 4 with Sunday = 0).
    let weekday = (days + 4).rem_euclid(7) as u32;
    LocalTime {
        month,
        day,
        hour: (secs / 3600) as u32,
        minute: ((secs % 3600) / 60) as u32,
        weekday,
    }
}

/// Captures the process timezone offset (seconds east of UTC) once.
pub fn local_utc_offset_seconds() -> i64 {
    // `TZ` itself carries no numeric offset on Windows; reading the offset
    // portably without chrono requires winapi plumbing. The host (Tauri
    // app) runs inside the user's session; `std::time::SystemTime` is UTC.
    // Pragmatic approach: derive the offset from the difference between
    // the C runtime's localtime and UTC via `std::process`-free probe is
    // unavailable, so use the `TZ` env when numeric, else 0 and let the
    // host pass its own offset when constructing the scheduler.
    std::env::var("BLACKAGENT_UTC_OFFSET_SECONDS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Next fire time for a cron expression, strictly after `after_unix`
/// (seconds). Scans forward minute by minute (bounded at 366 days).
pub fn next_cron_fire(
    expression: &str,
    after_unix: i64,
    utc_offset_seconds: i64,
) -> Result<Option<i64>, String> {
    let fields = parse_cron(expression)?;
    let matches = |field: &CronField, value: u32| match field {
        CronField::Set(values) => values.contains(&value),
    };
    let [minute_f, hour_f, dom_f, month_f, dow_f] = fields;
    // Start one minute after `after_unix`, aligned to the minute.
    let start = (after_unix / 60 + 1) * 60;
    let limit = start + 366 * 86_400;
    let mut candidate = start;
    while candidate < limit {
        let time = local_time(candidate, utc_offset_seconds);
        let dow = if time.weekday == 0 {
            0
        } else {
            time.weekday
        };
        let dow_matches = match &dow_f {
            // 7 normalizes to Sunday (0).
            CronField::Set(values) => values.contains(&dow) || (dow == 0 && values.contains(&7)),
        };
        if matches(&minute_f, time.minute)
            && matches(&hour_f, time.hour)
            && matches(&month_f, time.month)
            && matches(&dom_f, time.day)
            && dow_matches
        {
            return Ok(Some(candidate));
        }
        candidate += 60;
    }
    Ok(None)
}

/// Next fire time for an interval rule strictly after `after_unix`.
pub fn next_interval_fire(
    unit: IntervalUnit,
    step: u32,
    anchor_unix: i64,
    after_unix: i64,
) -> i64 {
    let duration_seconds: i64 = match unit {
        IntervalUnit::Minute => 60 * step as i64,
        IntervalUnit::Hourly => 3600 * step as i64,
        IntervalUnit::Daily => 86_400 * step as i64,
        IntervalUnit::Weekly => 604_800 * step as i64,
        IntervalUnit::Monthly => 2_629_800 * step as i64, // ~30.44 days
        IntervalUnit::Yearly => 31_557_600 * step as i64, // ~365.25 days
    };
    let mut next = anchor_unix + duration_seconds;
    while next <= after_unix {
        next += duration_seconds;
    }
    next
}

/// Host-side automation store the Cron tools call through. channel never
/// touches the database; the host implements persistence, quota (20 tasks)
/// and scheduling.
pub trait AutomationStore: Send + Sync {
    /// Creates an automation; returns a human-readable confirmation
    /// (including the id) or an error message (quota/invalid schedule).
    fn create(
        &self,
        title: String,
        prompt: String,
        schedule: ScheduleSpec,
        args: &Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
    /// Lists automations as human-readable lines.
    fn list(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
    /// Updates mutable fields (id/title/prompt/cron/recurring/maxRuns).
    fn update(
        &self,
        id: &str,
        title: String,
        args: &Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
    /// Deletes by id.
    fn delete(
        &self,
        id: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cron_validates_and_expands() {
        let fields = parse_cron("*/15 9-17 * * 1-5").unwrap();
        match &fields[0] {
            CronField::Set(values) => {
                assert_eq!(values.len(), 4);
                assert_eq!(values[0], 0);
                assert_eq!(values[3], 45);
            }
        }
        match &fields[1] {
            CronField::Set(values) => assert_eq!(values.len(), 9),
        }
        assert!(parse_cron("* * * *").is_err(), "4 fields rejected");
        assert!(parse_cron("60 * * * *").is_err(), "minute 60 out of range");
        assert!(parse_cron("* 24 * * *").is_err(), "hour 24 out of range");
        assert!(parse_cron("* * 0 * *").is_err(), "dom starts at 1");
        assert!(parse_cron("*/0 * * * *").is_err(), "zero step rejected");
        let dow = parse_cron("* * * * 0,7").unwrap();
        match &dow[4] {
            CronField::Set(values) => assert_eq!(values, &vec![0, 7]),
        }
    }

    #[test]
    fn schedule_spec_requires_exactly_one_form() {
        let ok = ScheduleSpec::from_tool_args(
            &serde_json::json!({"cron": "*/5 * * * *", "prompt": "p", "title": "t"}),
        )
        .unwrap();
        assert_eq!(
            ok,
            ScheduleSpec::Cron {
                expression: "*/5 * * * *".to_owned()
            }
        );
        let ok = ScheduleSpec::from_tool_args(
            &serde_json::json!({"delayMinutes": 8, "prompt": "p", "title": "t"}),
        )
        .unwrap();
        assert_eq!(ok, ScheduleSpec::Delay { minutes: 8 });
        let ok = ScheduleSpec::from_tool_args(&serde_json::json!({
            "intervalUnit": "minute", "interval": 20, "prompt": "p", "title": "t"
        }))
        .unwrap();
        assert_eq!(
            ok,
            ScheduleSpec::Interval {
                unit: IntervalUnit::Minute,
                step: 20
            }
        );
        // Two forms at once rejected.
        assert!(ScheduleSpec::from_tool_args(&serde_json::json!({
            "cron": "* * * * *", "delayMinutes": 5, "prompt": "p", "title": "t"
        }))
        .is_err());
        // No form rejected.
        assert!(ScheduleSpec::from_tool_args(&serde_json::json!({
            "prompt": "p", "title": "t"
        }))
        .is_err());
        // Out-of-range delay rejected.
        assert!(ScheduleSpec::from_tool_args(&serde_json::json!({
            "delayMinutes": 0, "prompt": "p", "title": "t"
        }))
        .is_err());
    }

    #[test]
    fn next_cron_fire_walks_forward_in_local_time() {
        // Every minute: next fire is exactly one minute later.
        let after = 1_700_000_000; // 2023-11-14T22:13:20Z
        let next = next_cron_fire("* * * * *", after, 0).unwrap().unwrap();
        assert_eq!(next, 1_700_000_040);

        // Hourly at minute 30.
        let next = next_cron_fire("30 * * * *", after, 0).unwrap().unwrap();
        assert_eq!(next % 3600, 1800);
        assert!(next > after);

        // Weekday-only field respects dow.
        let next = next_cron_fire("0 9 * * 1-5", after, 0).unwrap().unwrap();
        let time = local_time(next, 0);
        assert!(matches!(time.weekday, 1..=5));
        assert_eq!(time.hour, 9);
        assert_eq!(time.minute, 0);

        // Unreachable cron (Feb 31) yields None within the 366-day scan.
        let next = next_cron_fire("0 0 31 2 *", after, 0).unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn next_interval_fire_advances_by_steps() {
        let anchor = 1_700_000_000;
        let next = next_interval_fire(IntervalUnit::Minute, 20, anchor, anchor + 1);
        assert_eq!(next, anchor + 1200);
        // Skips ahead when `after` is far past the anchor.
        let after = anchor + 5 * 1200 + 30;
        let next = next_interval_fire(IntervalUnit::Minute, 20, anchor, after);
        assert_eq!(next, anchor + 6 * 1200);
    }
}
