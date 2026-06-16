//! PostgreSQL-compatible MONEY type.
//!
//! Storage: signed int64 cents (1/100 of the currency unit).

use crate::LimboError;

/// PostgreSQL-compatible money value stored as cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Money(pub i64);

impl Money {
    pub fn from_cents(cents: i64) -> Self {
        Self(cents)
    }

    pub fn cents(self) -> i64 {
        self.0
    }

    pub fn from_text(input: &str) -> Result<Self, LimboError> {
        parse_money(input)
    }

    pub fn to_text(self) -> String {
        format_money(self)
    }

    pub fn add(self, other: Self) -> Result<Self, LimboError> {
        let cents = self.0.checked_add(other.0).ok_or_overflow()?;
        Ok(Self(cents))
    }

    pub fn sub(self, other: Self) -> Result<Self, LimboError> {
        let cents = self.0.checked_sub(other.0).ok_or_overflow()?;
        Ok(Self(cents))
    }

    /// Scale money by a floating-point factor, rounding to nearest cent.
    pub fn mul(self, factor: f64) -> Result<Self, LimboError> {
        if !factor.is_finite() {
            return Err(invalid_money("non-finite factor"));
        }
        let scaled = (self.0 as f64 * factor).round();
        if scaled.abs() > i64::MAX as f64 {
            return Err(LimboError::IntegerOverflow);
        }
        Ok(Self(scaled as i64))
    }

    /// Divide money by a floating-point divisor, rounding to nearest cent.
    pub fn div(self, divisor: f64) -> Result<Self, LimboError> {
        if !divisor.is_finite() || divisor == 0.0 {
            return Err(LimboError::Constraint("division by zero".into()));
        }
        let scaled = (self.0 as f64 / divisor).round();
        if scaled.abs() > i64::MAX as f64 {
            return Err(LimboError::IntegerOverflow);
        }
        Ok(Self(scaled as i64))
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

fn parse_money(input: &str) -> Result<Money, LimboError> {
    let mut s = input.trim();
    if s.is_empty() {
        return Err(invalid_money("empty string"));
    }

    let mut negative = false;
    if let Some(inner) = s.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        negative = true;
        s = inner.trim();
    } else if let Some(rest) = s.strip_prefix('-') {
        negative = true;
        s = rest.trim();
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest.trim();
    }

    s = s.trim_start_matches('$').trim();

    if s.is_empty() {
        return Err(invalid_money(input));
    }

    let mut whole_part = String::new();
    let mut frac_part = String::new();
    let mut seen_dot = false;

    for ch in s.chars() {
        match ch {
            ',' => continue,
            '.' if !seen_dot => {
                seen_dot = true;
            }
            '.' => return Err(invalid_money(input)),
            c if c.is_ascii_digit() => {
                if seen_dot {
                    if frac_part.len() >= 2 {
                        return Err(invalid_money(input));
                    }
                    frac_part.push(c);
                } else {
                    whole_part.push(c);
                }
            }
            _ => return Err(invalid_money(input)),
        }
    }

    if whole_part.is_empty() {
        whole_part.push('0');
    }

    let dollars: i64 = whole_part
        .parse::<i64>()
        .map_err(|_| invalid_money(input))?
        .checked_mul(100)
        .ok_or(LimboError::IntegerOverflow)?;

    let cents = if frac_part.is_empty() {
        0
    } else {
        let padded = format!("{frac_part:<02}");
        padded[..2]
            .parse::<i64>()
            .map_err(|_| invalid_money(input))?
    };

    let mut total = dollars
        .checked_add(cents)
        .ok_or(LimboError::IntegerOverflow)?;

    if negative {
        total = total.checked_neg().ok_or(LimboError::IntegerOverflow)?;
    }

    Ok(Money(total))
}

fn format_money(m: Money) -> String {
    let negative = m.0 < 0;
    let abs = m.0.unsigned_abs();
    let dollars = abs / 100;
    let cents = abs % 100;

    let dollars_str = format_with_commas(dollars);
    let body = format!("${dollars_str}.{cents:02}");

    if negative {
        format!("({body})")
    } else {
        body
    }
}

fn format_with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn invalid_money(s: &str) -> LimboError {
    LimboError::Constraint(format!("invalid input syntax for type money: \"{s}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dollar_amount() {
        let m = Money::from_text("$12.34").unwrap();
        assert_eq!(m.cents(), 1234);
    }

    #[test]
    fn parse_with_commas() {
        let m = Money::from_text("$1,234.56").unwrap();
        assert_eq!(m.cents(), 123_456);
    }

    #[test]
    fn parse_negative_parens() {
        let m = Money::from_text("($1.00)").unwrap();
        assert_eq!(m.cents(), -100);
    }

    #[test]
    fn parse_plain_number() {
        let m = Money::from_text("42.50").unwrap();
        assert_eq!(m.cents(), 4250);
    }

    #[test]
    fn format_positive() {
        assert_eq!(Money(123_456).to_text(), "$1,234.56");
    }

    #[test]
    fn format_negative() {
        assert_eq!(Money(-100).to_text(), "($1.00)");
    }

    #[test]
    fn add_and_sub() {
        let a = Money::from_text("$1.00").unwrap();
        let b = Money::from_text("$0.50").unwrap();
        assert_eq!(a.add(b).unwrap().cents(), 150);
        assert_eq!(a.sub(b).unwrap().cents(), 50);
    }

    #[test]
    fn mul_rounds_to_cent() {
        let m = Money::from_text("$1.00").unwrap();
        assert_eq!(m.mul(1.5).unwrap().cents(), 150);
        assert_eq!(m.mul(0.333).unwrap().cents(), 33);
    }

    #[test]
    fn div_by_zero() {
        let m = Money::from_text("$1.00").unwrap();
        assert!(m.div(0.0).is_err());
    }

    #[test]
    fn text_roundtrip() {
        let inputs = ["$0.00", "$12.34", "($99.99)", "$1,000.01"];
        for input in inputs {
            let m = Money::from_text(input).unwrap();
            let back = Money::from_text(&m.to_text()).unwrap();
            assert_eq!(m, back, "roundtrip failed for {input}");
        }
    }
}
