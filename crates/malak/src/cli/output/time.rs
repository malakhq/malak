//! Format wall-clock times and elapsed ages for people.

use time::{OffsetDateTime, UtcOffset};

fn instant(ms: u64) -> Option<OffsetDateTime> {
    let utc = utc_instant(ms)?;
    Some(utc.to_offset(UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC)))
}

fn utc_instant(ms: u64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

pub(in crate::cli) fn timestamp(ms: Option<u64>, now: u64) -> String {
    let Some((ms, at)) = ms.and_then(|ms| instant(ms).map(|at| (ms, at))) else {
        return "Unavailable".into();
    };
    format!(
        "{} {:02}:{:02} {} ({})",
        at.date(),
        at.hour(),
        at.minute(),
        zone(at.offset()),
        age(ms, now)
    )
}

fn zone(offset: UtcOffset) -> String {
    let (h, m, _) = offset.as_hms();
    let sign = if offset.is_negative() { '-' } else { '+' };
    format!("{sign}{:02}:{:02}", h.abs(), m.abs())
}

pub(in crate::cli) fn clock(ms: Option<u64>) -> String {
    ms.and_then(instant).map_or_else(
        || "Unavailable".into(),
        |at| format!("{:02}:{:02}:{:02}", at.hour(), at.minute(), at.second()),
    )
}

pub(in crate::cli) fn age(ms: u64, now: u64) -> String {
    let seconds = ms.abs_diff(now) / 1000;
    let elapsed = match seconds {
        0..=59 => format!("{seconds} sec"),
        60..=3599 => format!("{} min", seconds / 60),
        3600..=86399 => format!("{} hr", seconds / 3600),
        _ => format!("{} d", seconds / 86400),
    };
    if ms > now {
        format!("in {elapsed}")
    } else {
        format!("{elapsed} ago")
    }
}

pub(in crate::cli) fn now_ms() -> u64 {
    OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .max(0)
        .div_euclid(1_000_000) as u64
}

pub(in crate::cli) fn local_zone(ms: u64) -> String {
    utc_instant(ms)
        .and_then(|at| UtcOffset::local_offset_at(at).ok())
        .map_or_else(
            || "UTC".into(),
            |offset| format!("local time, UTC{}", zone(offset)),
        )
}
