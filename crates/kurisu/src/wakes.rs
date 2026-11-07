//! Scheduled wakes: when to deliver a message to a session.
//! Times are parsed in the server's local time zone; `repeat` is a 5-field cron.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};

/// A wake that fires more than this many seconds after it was due is "late".
pub const LATE_AFTER: i64 = 60;

/// Parse `at` relative to `now` (unix seconds): `in 30m` / `in 2h 15m` / `in 1d` / `90s`,
/// RFC 3339 (`2026-11-07T18:00:00Z`, `…-05:00`), local `YYYY-MM-DD HH:MM[:SS]` (or with `T`),
/// or local `HH:MM` (the next time that clock time comes round).
pub fn parse_at(at: &str, now: i64) -> Result<i64> {
    let t = at.trim();
    let rel = t.strip_prefix("in ").unwrap_or(t).trim();
    if let Some(secs) = parse_duration(rel) {
        return Ok(now + secs);
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(t) {
        return Ok(d.timestamp());
    }
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(t, f) {
            return local(n);
        }
    }
    for f in ["%H:%M", "%H:%M:%S"] {
        if let Ok(tm) = NaiveTime::parse_from_str(t, f) {
            let today = Local.timestamp_opt(now, 0).single().ok_or_else(|| anyhow!("bad clock"))?.date_naive();
            let mut ts = local(today.and_time(tm))?;
            if ts <= now {
                ts = local((today + Duration::days(1)).and_time(tm))?;
            }
            return Ok(ts);
        }
    }
    bail!("cannot read the time {at:?}: use 'in 30m', 'in 2h', '18:30', '2026-11-08 06:00' (local) or RFC 3339 with an offset")
}

fn local(n: NaiveDateTime) -> Result<i64> {
    Local.from_local_datetime(&n).earliest().map(|d| d.timestamp()).ok_or_else(|| anyhow!("{n} does not exist in the local time zone"))
}

/// `30m`, `2h 15m`, `1d`, `90s`, `45` (minutes): seconds.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.replace(' ', "");
    if s.is_empty() {
        return None;
    }
    if let Ok(m) = s.parse::<i64>() {
        return (m > 0).then_some(m * 60);
    }
    let mut total = 0i64;
    let mut num = String::new();
    let mut any = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let mut unit = c.to_string();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphabetic() {
                unit.push(n);
                chars.next();
            } else {
                break;
            }
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        total += n * match unit.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86400,
            _ => return None,
        };
        any = true;
    }
    (any && num.is_empty() && total > 0).then_some(total)
}

/// A 5-field cron expression (minute hour day-of-month month day-of-week), local time.
/// Fields take `*`, numbers, `a-b`, lists `a,b` and steps `*/n` / `a-b/n`; day-of-week 0 or 7 is Sunday.
#[derive(Debug, Clone)]
pub struct Cron {
    min: Vec<bool>,
    hour: Vec<bool>,
    dom: Vec<bool>,
    month: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            bail!("repeat must be a 5-field cron (minute hour day month weekday), e.g. '0 6 * * *' for 06:00 daily");
        }
        let mut dow = field(f[4], 0, 7)?;
        if dow[7] {
            dow[0] = true;
        }
        Ok(Self {
            min: field(f[0], 0, 59)?,
            hour: field(f[1], 0, 23)?,
            dom: field(f[2], 1, 31)?,
            month: field(f[3], 1, 12)?,
            dow,
            dom_any: f[2] == "*",
            dow_any: f[4] == "*",
        })
    }

    fn matches(&self, t: &DateTime<Local>) -> bool {
        let dom = self.dom[t.day() as usize];
        let dow = self.dow[t.weekday().num_days_from_sunday() as usize];
        // Classic cron: if both day fields are restricted, either may match.
        let day = match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        };
        self.min[t.minute() as usize] && self.hour[t.hour() as usize] && self.month[t.month() as usize] && day
    }

    /// The first matching minute strictly after `after` (unix seconds), within about 4 years.
    pub fn next_after(&self, after: i64) -> Option<i64> {
        let start = Local.timestamp_opt(after - after.rem_euclid(60) + 60, 0).single()?;
        let mut day: NaiveDate = start.date_naive();
        let end = day + Duration::days(366 * 4);
        let mut first = true;
        while day <= end {
            if self.month[day.month() as usize] {
                for h in 0..24u32 {
                    if !self.hour[h as usize] {
                        continue;
                    }
                    for m in 0..60u32 {
                        if !self.min[m as usize] {
                            continue;
                        }
                        let Some(t) = Local.from_local_datetime(&day.and_hms_opt(h, m, 0)?).earliest() else { continue };
                        if first && t < start {
                            continue;
                        }
                        if self.matches(&t) {
                            return Some(t.timestamp());
                        }
                    }
                }
            }
            first = false;
            day += Duration::days(1);
        }
        None
    }
}

fn field(s: &str, lo: u32, hi: u32) -> Result<Vec<bool>> {
    let mut v = vec![false; hi as usize + 1];
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| anyhow!("bad cron step in {s:?}"))?),
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (num(a, lo, hi)?, num(b, lo, hi)?)
        } else {
            let a = num(range, lo, hi)?;
            (a, if part.contains('/') { hi } else { a })
        };
        if a > b {
            bail!("bad cron range {range:?}");
        }
        let mut i = a;
        while i <= b {
            v[i as usize] = true;
            i += step;
        }
    }
    Ok(v)
}

fn num(s: &str, lo: u32, hi: u32) -> Result<u32> {
    s.parse::<u32>().ok().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| anyhow!("cron value {s:?} must be {lo}-{hi}"))
}

/// Local, human-readable time for messages.
pub fn show(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d %H:%M %Z").to_string()).unwrap_or_else(|| ts.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_absolute_and_clock_times() {
        let now = 1_800_000_000;
        assert_eq!(parse_at("in 30m", now).unwrap(), now + 1800);
        assert_eq!(parse_at("2h 15m", now).unwrap(), now + 8100);
        assert_eq!(parse_at("in 1d", now).unwrap(), now + 86400);
        assert_eq!(parse_at("90s", now).unwrap(), now + 90);
        assert_eq!(parse_at("45", now).unwrap(), now + 2700);
        assert_eq!(parse_at("2027-01-15T12:00:00Z", now).unwrap(), 1_800_014_400);
        assert_eq!(parse_at("2027-01-15T07:00:00-05:00", now).unwrap(), 1_800_014_400);
        let l = parse_at("2027-01-15 12:00", now).unwrap();
        assert_eq!(Local.timestamp_opt(l, 0).unwrap().hour(), 12);
        let c = parse_at("06:00", now).unwrap();
        assert!(c > now && c <= now + 86400);
        assert_eq!(Local.timestamp_opt(c, 0).unwrap().format("%H:%M").to_string(), "06:00");
        for bad in ["soon", "in 3 fortnights", "25:00", "", "in 0m"] {
            assert!(parse_at(bad, now).is_err(), "{bad}");
        }
    }

    #[test]
    fn cron_next() {
        let daily6 = Cron::parse("0 6 * * *").unwrap();
        let base = parse_at("2027-01-15 05:59", 0).unwrap();
        assert_eq!(daily6.next_after(base).unwrap(), parse_at("2027-01-15 06:00", 0).unwrap());
        assert_eq!(daily6.next_after(parse_at("2027-01-15 06:00", 0).unwrap()).unwrap(), parse_at("2027-01-16 06:00", 0).unwrap());
        // 2027-01-15 is a Friday: weekdays at 09:30 skip to Monday the 18th.
        let wk = Cron::parse("30 9 * * 1-5").unwrap();
        assert_eq!(wk.next_after(parse_at("2027-01-15 10:00", 0).unwrap()).unwrap(), parse_at("2027-01-18 09:30", 0).unwrap());
        let every15 = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(every15.next_after(parse_at("2027-01-15 10:01", 0).unwrap()).unwrap(), parse_at("2027-01-15 10:15", 0).unwrap());
        let sunday = Cron::parse("0 8 * * 7").unwrap();
        assert_eq!(sunday.next_after(parse_at("2027-01-15 10:00", 0).unwrap()).unwrap(), parse_at("2027-01-17 08:00", 0).unwrap());
        for bad in ["0 6 * *", "60 * * * *", "* * * * 8", "*/0 * * * *", "5-1 * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }
}
