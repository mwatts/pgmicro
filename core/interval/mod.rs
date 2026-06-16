//! PostgreSQL-compatible INTERVAL type.
//!
//! Storage layout (16-byte little-endian blob):
//!   months: i32
//!   days: i32
//!   microseconds: i64

use crate::LimboError;

/// Seconds per day (PostgreSQL `SECS_PER_DAY`).
const SECS_PER_DAY: f64 = 86_400.0;
/// Average days per month used by PostgreSQL `extract(epoch from interval)`.
const DAYS_PER_MONTH: f64 = 365.25 / 12.0;
const USECS_PER_SEC: i64 = 1_000_000;
const USECS_PER_DAY: i64 = SECS_PER_DAY as i64 * USECS_PER_SEC;

/// PostgreSQL-compatible interval (three-field representation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub microseconds: i64,
}

impl Interval {
    pub const BLOB_LEN: usize = 16;

    pub fn from_blob(bytes: &[u8]) -> Result<Self, LimboError> {
        if bytes.len() != Self::BLOB_LEN {
            return Err(LimboError::Constraint(format!(
                "interval blob must be {} bytes, got {}",
                Self::BLOB_LEN,
                bytes.len()
            )));
        }
        let months = i32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let days = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let microseconds = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
        Ok(Self {
            months,
            days,
            microseconds,
        })
    }

    pub fn to_blob(self) -> [u8; Self::BLOB_LEN] {
        let mut out = [0u8; Self::BLOB_LEN];
        out[0..4].copy_from_slice(&self.months.to_le_bytes());
        out[4..8].copy_from_slice(&self.days.to_le_bytes());
        out[8..16].copy_from_slice(&self.microseconds.to_le_bytes());
        out
    }

    pub fn from_text(input: &str) -> Result<Self, LimboError> {
        parse_interval(input)
    }

    pub fn to_text(self) -> String {
        format_interval(self)
    }

    /// Field-wise addition (PostgreSQL interval + interval).
    pub fn add(self, other: Self) -> Result<Self, LimboError> {
        let months = self.months.checked_add(other.months).ok_or_overflow()?;
        let days = self.days.checked_add(other.days).ok_or_overflow()?;
        let microseconds = self
            .microseconds
            .checked_add(other.microseconds)
            .ok_or_overflow()?;
        Ok(Self {
            months,
            days,
            microseconds,
        })
    }

    pub fn sub(self, other: Self) -> Result<Self, LimboError> {
        let months = self.months.checked_sub(other.months).ok_or_overflow()?;
        let days = self.days.checked_sub(other.days).ok_or_overflow()?;
        let microseconds = self
            .microseconds
            .checked_sub(other.microseconds)
            .ok_or_overflow()?;
        Ok(Self {
            months,
            days,
            microseconds,
        })
    }

    pub fn mul(self, factor: f64) -> Result<Self, LimboError> {
        if !factor.is_finite() {
            return Err(LimboError::Constraint(
                "invalid input syntax for type interval".into(),
            ));
        }
        scale_interval(self, factor)
    }

    pub fn div(self, divisor: f64) -> Result<Self, LimboError> {
        if !divisor.is_finite() || divisor == 0.0 {
            return Err(LimboError::Constraint("division by zero".into()));
        }
        scale_interval(self, 1.0 / divisor)
    }

    pub fn negate(self) -> Self {
        Self {
            months: -self.months,
            days: -self.days,
            microseconds: -self.microseconds,
        }
    }

    /// `justify_days`: convert 30-day month units into days.
    pub fn justify_days(self) -> Self {
        Self {
            months: 0,
            days: self.days + self.months.saturating_mul(30),
            microseconds: self.microseconds,
        }
    }

    /// `justify_hours`: convert whole days into the time (microseconds) field.
    pub fn justify_hours(mut self) -> Self {
        if self.days != 0 {
            self.microseconds = self
                .microseconds
                .saturating_add(i64::from(self.days) * USECS_PER_DAY);
            self.days = 0;
        }
        self
    }

    /// Total seconds for `extract(epoch FROM interval)` (PostgreSQL formula).
    pub fn to_epoch_seconds(self) -> f64 {
        f64::from(self.months) * DAYS_PER_MONTH * SECS_PER_DAY
            + f64::from(self.days) * SECS_PER_DAY
            + self.microseconds as f64 / USECS_PER_SEC as f64
    }

    pub fn extract_field(self, field: &str) -> Result<f64, LimboError> {
        let field = field.to_ascii_lowercase();
        match field.as_str() {
            "epoch" => Ok(self.to_epoch_seconds()),
            "year" | "years" => Ok((self.months / 12) as f64),
            "month" | "months" => Ok((self.months % 12).unsigned_abs() as f64),
            "day" | "days" => Ok(self.days as f64),
            "hour" | "hours" => Ok((self.microseconds / 3_600_000_000) as f64),
            "minute" | "minutes" => Ok(((self.microseconds / 60_000_000) % 60) as f64),
            "second" | "seconds" => Ok(((self.microseconds / 1_000_000) % 60) as f64),
            "millisecond" | "milliseconds" | "ms" => {
                Ok(((self.microseconds / 1_000) % 1_000) as f64)
            }
            "microsecond" | "microseconds" | "us" => Ok((self.microseconds % 1_000_000) as f64),
            other => Err(LimboError::Constraint(format!(
                "invalid extract field for interval: {other}"
            ))),
        }
    }
}

trait OverflowExt<T> {
    fn ok_or_overflow(self) -> Result<T, LimboError>;
}

impl<T> OverflowExt<T> for Option<T> {
    fn ok_or_overflow(self) -> Result<T, LimboError> {
        self.ok_or(LimboError::IntegerOverflow)
    }
}

fn scale_interval(iv: Interval, factor: f64) -> Result<Interval, LimboError> {
    let months = (f64::from(iv.months) * factor).round();
    let days = (f64::from(iv.days) * factor).round();
    let microseconds = (iv.microseconds as f64 * factor).round();

    if months.abs() > i32::MAX as f64
        || days.abs() > i32::MAX as f64
        || microseconds.abs() > i64::MAX as f64
    {
        return Err(LimboError::IntegerOverflow);
    }

    Ok(Interval {
        months: months as i32,
        days: days as i32,
        microseconds: microseconds as i64,
    })
}

fn parse_interval(input: &str) -> Result<Interval, LimboError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(LimboError::Constraint(
            "invalid input syntax for type interval: empty string".into(),
        ));
    }

    // ISO 8601 duration: PnYnMnDTnHnMnS
    if s.starts_with('P') || s.starts_with('p') {
        return parse_iso8601_duration(s);
    }

    // SQL standard day-time: [ @ ] [ sign ] days hours:minutes:seconds[.frac]
    if looks_like_day_time(s) {
        return parse_day_time(s);
    }

    parse_field_list(s)
}

fn looks_like_day_time(s: &str) -> bool {
    let t = s.strip_prefix('@').unwrap_or(s).trim();
    t.contains(':') || {
        let parts: Vec<_> = t.split_whitespace().collect();
        parts.len() == 1
            && parts[0]
                .chars()
                .all(|c| c.is_ascii_digit() || c == '-' || c == '+')
    }
}

fn parse_day_time(s: &str) -> Result<Interval, LimboError> {
    let mut rest = s.trim();
    if let Some(r) = rest.strip_prefix('@') {
        rest = r.trim();
    }

    let (sign, rest) = parse_optional_sign(rest);
    let sign = if sign { -1 } else { 1 };

    if rest.contains(':') {
        let (day_part, time_part) = match rest.split_once(' ') {
            Some((d, t)) => (d, t),
            None => ("0", rest),
        };
        let days: i32 = day_part
            .parse::<i32>()
            .map_err(|_| invalid_interval(rest))?
            .checked_mul(sign)
            .ok_or(LimboError::IntegerOverflow)?;
        let microseconds = parse_hms_to_microseconds(time_part)? * i64::from(sign);
        return Ok(Interval {
            months: 0,
            days,
            microseconds,
        });
    }

    let days: i32 = rest
        .parse::<i32>()
        .map_err(|_| invalid_interval(rest))?
        .checked_mul(sign)
        .ok_or(LimboError::IntegerOverflow)?;
    Ok(Interval {
        months: 0,
        days,
        microseconds: 0,
    })
}

fn parse_hms_to_microseconds(s: &str) -> Result<i64, LimboError> {
    let (hms, frac) = match s.split_once('.') {
        Some((h, f)) => (h, Some(f)),
        None => (s, None),
    };
    let parts: Vec<_> = hms.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(invalid_interval(s));
    }
    let hours: i64 = parts[0].parse().map_err(|_| invalid_interval(s))?;
    let minutes: i64 = parts[1].parse().map_err(|_| invalid_interval(s))?;
    let seconds: i64 = if parts.len() == 3 {
        parts[2].parse().map_err(|_| invalid_interval(s))?
    } else {
        0
    };
    let mut microseconds = (hours * 3600 + minutes * 60 + seconds) * USECS_PER_SEC;
    if let Some(frac) = frac {
        let frac_digits = frac.len().min(6);
        let padded = format!("{:0<6}", frac);
        let frac_us: i64 = padded[..frac_digits]
            .parse()
            .map_err(|_| invalid_interval(s))?;
        let scale = 10_i64.pow((6 - frac_digits) as u32);
        microseconds += frac_us * scale;
    }
    Ok(microseconds)
}

fn parse_iso8601_duration(s: &str) -> Result<Interval, LimboError> {
    let s = &s[1..]; // drop P
    let (date_part, time_part) = match s.split_once('T') {
        Some((d, t)) => (Some(d), Some(t)),
        None => (Some(s), None),
    };

    let mut months = 0i32;
    let mut days = 0i32;
    let mut microseconds = 0i64;

    if let Some(date_part) = date_part {
        let mut num = String::new();
        for ch in date_part.chars() {
            if ch.is_ascii_digit() {
                num.push(ch);
            } else {
                let n: i32 = num.parse().map_err(|_| invalid_interval(s))?;
                num.clear();
                match ch {
                    'Y' | 'y' => {
                        months = months
                            .checked_add(n.checked_mul(12).ok_or_overflow()?)
                            .ok_or_overflow()?;
                    }
                    'M' | 'm' => months = months.checked_add(n).ok_or_overflow()?,
                    'D' | 'd' => days = days.checked_add(n).ok_or_overflow()?,
                    'W' | 'w' => {
                        days = days
                            .checked_add(n.checked_mul(7).ok_or_overflow()?)
                            .ok_or_overflow()?;
                    }
                    _ => return Err(invalid_interval(s)),
                }
            }
        }
        if !num.is_empty() {
            return Err(invalid_interval(s));
        }
    }

    if let Some(time_part) = time_part {
        let mut num = String::new();
        for ch in time_part.chars() {
            if ch.is_ascii_digit() || ch == '.' {
                num.push(ch);
            } else {
                let n: f64 = num.parse().map_err(|_| invalid_interval(s))?;
                num.clear();
                let us = match ch {
                    'H' | 'h' => (n * 3_600_000_000.0).round() as i64,
                    'M' | 'm' => (n * 60_000_000.0).round() as i64,
                    'S' | 's' => (n * 1_000_000.0).round() as i64,
                    _ => return Err(invalid_interval(s)),
                };
                microseconds = microseconds.checked_add(us).ok_or_overflow()?;
            }
        }
        if !num.is_empty() {
            return Err(invalid_interval(s));
        }
    }

    Ok(Interval {
        months,
        days,
        microseconds,
    })
}

fn parse_field_list(s: &str) -> Result<Interval, LimboError> {
    let mut months = 0i32;
    let mut days = 0i32;
    let mut microseconds = 0i64;
    let mut sign = 1i32;
    let mut i = 0;
    let chars: Vec<char> = s.chars().collect();

    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        if chars[i] == '+' {
            sign = 1;
            i += 1;
            continue;
        }
        if chars[i] == '-' {
            sign = -1;
            i += 1;
            continue;
        }

        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
            i += 1;
        }
        if start == i {
            return Err(invalid_interval(s));
        }
        let num_str: String = chars[start..i].iter().collect();
        let n: f64 = num_str.parse().map_err(|_| invalid_interval(s))?;

        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let unit_start = i;
        while i < chars.len() && chars[i].is_alphabetic() {
            i += 1;
        }
        if unit_start == i {
            return Err(invalid_interval(s));
        }
        let unit: String = chars[unit_start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        apply_unit(&unit, n, sign, &mut months, &mut days, &mut microseconds)?;
    }

    Ok(Interval {
        months,
        days,
        microseconds,
    })
}

fn apply_unit(
    unit: &str,
    n: f64,
    sign: i32,
    months: &mut i32,
    days: &mut i32,
    microseconds: &mut i64,
) -> Result<(), LimboError> {
    let sign_f = f64::from(sign);
    match unit {
        "year" | "years" | "yr" | "yrs" | "y" => {
            let m = (n * 12.0 * sign_f).round() as i32;
            *months = months.checked_add(m).ok_or_overflow()?;
        }
        "month" | "months" | "mon" | "mons" => {
            let m = (n * sign_f).round() as i32;
            *months = months.checked_add(m).ok_or_overflow()?;
        }
        "week" | "weeks" | "w" => {
            let d = (n * 7.0 * sign_f).round() as i32;
            *days = days.checked_add(d).ok_or_overflow()?;
        }
        "day" | "days" | "d" => {
            let d = (n * sign_f).round() as i32;
            *days = days.checked_add(d).ok_or_overflow()?;
        }
        "hour" | "hours" | "hr" | "hrs" | "h" => {
            let us = (n * 3_600_000_000.0 * sign_f).round() as i64;
            *microseconds = microseconds.checked_add(us).ok_or_overflow()?;
        }
        "minute" | "minutes" | "min" | "mins" | "m" => {
            let us = (n * 60_000_000.0 * sign_f).round() as i64;
            *microseconds = microseconds.checked_add(us).ok_or_overflow()?;
        }
        "second" | "seconds" | "sec" | "secs" | "s" => {
            let us = (n * 1_000_000.0 * sign_f).round() as i64;
            *microseconds = microseconds.checked_add(us).ok_or_overflow()?;
        }
        "millisecond" | "milliseconds" | "ms" => {
            let us = (n * 1_000.0 * sign_f).round() as i64;
            *microseconds = microseconds.checked_add(us).ok_or_overflow()?;
        }
        "microsecond" | "microseconds" | "us" => {
            let us = (n * sign_f).round() as i64;
            *microseconds = microseconds.checked_add(us).ok_or_overflow()?;
        }
        _ => return Err(invalid_interval(unit)),
    }
    Ok(())
}

fn parse_optional_sign(s: &str) -> (bool, &str) {
    if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
        (false, rest)
    } else {
        (false, s)
    }
}

fn format_interval(iv: Interval) -> String {
    if iv.months == 0 && iv.days == 0 && iv.microseconds == 0 {
        return "00:00:00".to_string();
    }

    let mut parts = Vec::new();
    if iv.months != 0 {
        let years = iv.months / 12;
        let mons = iv.months % 12;
        if years != 0 {
            parts.push(format!("{years} year{}", if years == 1 { "" } else { "s" }));
        }
        if mons != 0 {
            parts.push(format!("{mons} mon{}", if mons == 1 { "" } else { "s" }));
        }
    }
    if iv.days != 0 {
        parts.push(format!(
            "{} day{}",
            iv.days,
            if iv.days == 1 { "" } else { "s" }
        ));
    }
    if iv.microseconds != 0 {
        let abs = iv.microseconds.unsigned_abs();
        let hours = abs / 3_600_000_000;
        let minutes = (abs / 60_000_000) % 60;
        let seconds = (abs / 1_000_000) % 60;
        let frac = abs % 1_000_000;
        if frac == 0 {
            parts.push(format!("{hours}:{minutes:02}:{seconds:02}"));
        } else {
            parts.push(format!("{hours}:{minutes:02}:{seconds:02}.{frac:06}"));
        }
        if iv.microseconds < 0 {
            if let Some(last) = parts.last_mut() {
                *last = format!("-{last}");
            }
        }
    }
    parts.join(" ")
}

fn invalid_interval(s: &str) -> LimboError {
    LimboError::Constraint(format!("invalid input syntax for type interval: \"{s}\""))
}

/// Apply calendar-aware `timestamp + interval` (text in, text out).
pub fn timestamp_pl_interval(timestamp: &str, interval_blob: &[u8]) -> Result<String, LimboError> {
    timestamp_add_interval(timestamp, interval_blob, 1)
}

pub fn timestamp_mi_interval(timestamp: &str, interval_blob: &[u8]) -> Result<String, LimboError> {
    timestamp_add_interval(timestamp, interval_blob, -1)
}

fn timestamp_add_interval(
    timestamp: &str,
    interval_blob: &[u8],
    sign: i32,
) -> Result<String, LimboError> {
    use chrono::NaiveDateTime;

    let iv = Interval::from_blob(interval_blob)?;
    let months = iv.months.saturating_mul(sign);
    let days = iv.days.saturating_mul(sign);
    let microseconds = iv.microseconds.saturating_mul(i64::from(sign));

    let ts = timestamp.trim();
    let fmt = if ts.contains('.') {
        "%Y-%m-%d %H:%M:%S%.f"
    } else {
        "%Y-%m-%d %H:%M:%S"
    };
    let mut dt = NaiveDateTime::parse_from_str(ts, fmt).map_err(|_| {
        LimboError::Constraint(format!("invalid input syntax for type timestamp: \"{ts}\""))
    })?;

    if months != 0 {
        use chrono::Months;
        let date = dt.date();
        let new_date = if months > 0 {
            date.checked_add_months(Months::new(months as u32))
        } else {
            date.checked_sub_months(Months::new(months.unsigned_abs()))
        }
        .ok_or_else(|| LimboError::Constraint("timestamp out of range".into()))?;
        dt = new_date.and_time(dt.time());
    }

    if days != 0 {
        dt = dt
            .checked_add_signed(chrono::TimeDelta::days(i64::from(days)))
            .ok_or(LimboError::IntegerOverflow)?;
    }

    if microseconds != 0 {
        dt = dt
            .checked_add_signed(chrono::TimeDelta::microseconds(microseconds))
            .ok_or(LimboError::IntegerOverflow)?;
    }

    if ts.contains('.') {
        Ok(dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
    } else {
        Ok(dt.format("%Y-%m-%d %H:%M:%S").to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_one_day() {
        let iv = Interval::from_text("1 day").unwrap();
        assert_eq!(iv.days, 1);
        assert_eq!(iv.months, 0);
        assert_eq!(iv.microseconds, 0);
    }

    #[test]
    fn parse_month_and_day() {
        let iv = Interval::from_text("2 months 3 days").unwrap();
        assert_eq!(iv.months, 2);
        assert_eq!(iv.days, 3);
    }

    #[test]
    fn month_addition_is_field_wise() {
        let a = Interval::from_text("1 month").unwrap();
        let b = Interval::from_text("1 month").unwrap();
        let sum = a.add(b).unwrap();
        assert_eq!(sum.months, 2);
        assert_eq!(sum.days, 0);
        assert_ne!(sum, Interval::from_text("60 days").unwrap());
    }

    #[test]
    fn day_addition() {
        let a = Interval::from_text("30 days").unwrap();
        let b = Interval::from_text("30 days").unwrap();
        let sum = a.add(b).unwrap();
        assert_eq!(sum.days, 60);
        assert_eq!(sum.months, 0);
    }

    #[test]
    fn justify_days_converts_months() {
        let iv = Interval::from_text("1 month").unwrap();
        let j = iv.justify_days();
        assert_eq!(j.months, 0);
        assert_eq!(j.days, 30);
    }

    #[test]
    fn justify_hours_converts_days() {
        let iv = Interval::from_text("1 day").unwrap();
        let j = iv.justify_hours();
        assert_eq!(j.days, 0);
        assert_eq!(j.microseconds, USECS_PER_DAY);
    }

    #[test]
    fn epoch_one_month() {
        let iv = Interval::from_text("1 month").unwrap();
        let epoch = iv.to_epoch_seconds();
        let expected = DAYS_PER_MONTH * SECS_PER_DAY;
        assert!((epoch - expected).abs() < 0.001);
    }

    #[test]
    fn extract_epoch_and_day() {
        let iv = Interval::from_text("2 days 3 hours").unwrap();
        assert_eq!(iv.extract_field("day").unwrap(), 2.0);
        assert_eq!(iv.extract_field("hour").unwrap(), 3.0);
    }

    #[test]
    fn blob_roundtrip() {
        let iv = Interval::from_text("1 year 2 mons 3 days 4 hours").unwrap();
        let blob = iv.to_blob();
        let back = Interval::from_blob(&blob).unwrap();
        assert_eq!(iv, back);
    }

    #[test]
    fn timestamp_plus_one_month_calendar() {
        let iv = Interval::from_text("1 month").unwrap();
        let out = timestamp_pl_interval("2024-01-31 12:00:00", &iv.to_blob()).unwrap();
        assert_eq!(out, "2024-02-29 12:00:00");
    }

    #[test]
    fn timestamp_minus_one_day() {
        let iv = Interval::from_text("1 day").unwrap();
        let out = timestamp_mi_interval("2024-03-02 00:00:00", &iv.to_blob()).unwrap();
        assert_eq!(out, "2024-03-01 00:00:00");
    }
}
