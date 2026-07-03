use crate::ext::register_scalar_function;
use crate::types::Value;
use crate::{Connection, LimboError};
use turso_ext::{scalar, ExtensionApi, Value as ExtValue};

/// Register PostgreSQL-compatible scalar functions.
///
/// These are thin wrappers that map common PG function names to their Turso
/// equivalents, so that `DEFAULT now()`, `SELECT clock_timestamp()`, etc. work
/// without relying solely on translator-level rewriting.
pub fn register_pg_functions(ext_api: &mut ExtensionApi) {
    unsafe {
        register_scalar_function(ext_api.ctx, c"now".as_ptr(), pg_now);
        register_scalar_function(ext_api.ctx, c"clock_timestamp".as_ptr(), pg_now);
        register_scalar_function(ext_api.ctx, c"transaction_timestamp".as_ptr(), pg_now);
        register_scalar_function(ext_api.ctx, c"statement_timestamp".as_ptr(), pg_now);
    }
}

/// Returns the current timestamp as `YYYY-MM-DD HH:MM:SS.mmm`.
///
/// This is the Turso equivalent of PostgreSQL's `now()`, `clock_timestamp()`,
/// `transaction_timestamp()`, and `statement_timestamp()`. All four are mapped
/// to the same implementation since Turso does not distinguish between
/// transaction-time and wall-clock time.
#[scalar(name = "now")]
fn pg_now(_args: &[ExtValue]) -> ExtValue {
    let now = chrono::Utc::now();
    let formatted = now.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    ExtValue::from_text(formatted)
}

pub fn exec_pg_get_user_by_id(conn: &Connection, oid: i64) -> Value {
    crate::pg_role::exec_pg_get_user_by_id(conn, oid)
}

pub fn exec_pg_is_visible(_oid: i64) -> Value {
    Value::from_i64(1)
}

pub fn exec_pg_encoding_to_char(encoding: i64) -> Value {
    let name = match encoding {
        6 => "UTF8",
        0 => "SQL_ASCII",
        _ => "",
    };
    Value::build_text(name)
}

pub fn exec_pg_get_constraintdef(conn: &Connection, oid: i64) -> Value {
    match crate::pg_catalog::pg_get_constraintdef(conn, oid) {
        Some(s) => Value::build_text(s),
        None => Value::Null,
    }
}

pub fn exec_pg_get_indexdef(conn: &Connection, oid: i64) -> Value {
    match crate::pg_catalog::pg_get_indexdef(conn, oid) {
        Some(s) => Value::build_text(s),
        None => Value::Null,
    }
}

pub fn exec_pg_format_type(type_oid: i64, typemod: i64) -> Value {
    let type_name = match type_oid {
        16 => "boolean".to_string(),
        17 => "bytea".to_string(),
        18 => "\"char\"".to_string(),
        19 => "name".to_string(),
        20 => "bigint".to_string(),
        21 => "smallint".to_string(),
        23 => "integer".to_string(),
        25 => "text".to_string(),
        26 => "oid".to_string(),
        114 => "json".to_string(),
        700 => "real".to_string(),
        701 => "double precision".to_string(),
        1000 => "boolean[]".to_string(),
        1007 => "integer[]".to_string(),
        1009 => "text[]".to_string(),
        1022 => "double precision[]".to_string(),
        1042 => {
            if typemod > 4 {
                format!("character({})", typemod - 4)
            } else {
                "character".to_string()
            }
        }
        1043 => {
            if typemod > 4 {
                format!("character varying({})", typemod - 4)
            } else {
                "character varying".to_string()
            }
        }
        1082 => "date".to_string(),
        1083 => "time without time zone".to_string(),
        1114 => "timestamp without time zone".to_string(),
        1184 => "timestamp with time zone".to_string(),
        1186 => "interval".to_string(),
        790 => "money".to_string(),
        1700 => {
            if typemod > 4 {
                let precision = ((typemod - 4) >> 16) & 0xffff;
                let scale = (typemod - 4) & 0xffff;
                format!("numeric({precision},{scale})")
            } else {
                "numeric".to_string()
            }
        }
        2205 => "regclass".to_string(),
        2206 => "regtype".to_string(),
        2278 => "void".to_string(),
        2950 => "uuid".to_string(),
        3802 => "jsonb".to_string(),
        _ => "unknown".to_string(),
    };
    Value::build_text(type_name)
}

/// PostgreSQL's varlena MaxAllocSize (1GB - 1 byte). repeat()/lpad()/rpad()
/// raise "requested length too large" instead of allocating past this.
const PG_MAX_STRING_LEN: usize = 1_073_741_823;

fn check_pg_string_length(len: usize) -> Result<(), LimboError> {
    if len > PG_MAX_STRING_LEN {
        return Err(LimboError::InvalidArgument(
            "requested length too large".to_string(),
        ));
    }
    Ok(())
}

pub fn exec_lpad(input: &Value, length: usize, fill: &str) -> Result<Value, LimboError> {
    check_pg_string_length(length)?;
    let s = match input {
        Value::Text(t) => t.to_string(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let char_count = s.chars().count();
    if char_count >= length {
        Ok(Value::build_text(
            s.chars().take(length).collect::<String>(),
        ))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            Ok(Value::build_text(s))
        } else {
            let needed = length - char_count;
            let max_char_bytes = fill_chars.iter().map(|c| c.len_utf8()).max().unwrap_or(1);
            let worst_case_pad_bytes = needed.checked_mul(max_char_bytes).ok_or_else(|| {
                LimboError::InvalidArgument("requested length too large".to_string())
            })?;
            check_pg_string_length(worst_case_pad_bytes.saturating_add(s.len()))?;
            let pad: String = fill_chars.iter().cycle().take(needed).collect();
            Ok(Value::build_text(format!("{pad}{s}")))
        }
    }
}

fn gcd_inner(mut a: i64, mut b: i64) -> Result<i64, LimboError> {
    while b != 0 {
        let t = b;
        b = a.checked_rem(b).ok_or(LimboError::IntegerOverflow)?;
        a = t;
    }
    a.checked_abs().ok_or(LimboError::IntegerOverflow)
}

/// Greatest common divisor.
pub fn exec_gcd(a: i64, b: i64) -> Result<Value, LimboError> {
    Ok(Value::from_i64(gcd_inner(a, b)?))
}

/// Least common multiple.
pub fn exec_lcm(a: i64, b: i64) -> Result<Value, LimboError> {
    if a == 0 || b == 0 {
        return Ok(Value::from_i64(0));
    }
    let g = gcd_inner(a, b)?; // g is always > 0, so a/g never hits MIN/-1
    let b_abs = b.checked_abs().ok_or(LimboError::IntegerOverflow)?;
    let product = (a / g)
        .checked_mul(b_abs)
        .ok_or(LimboError::IntegerOverflow)?;
    product
        .checked_abs()
        .map(Value::from_i64)
        .ok_or(LimboError::IntegerOverflow)
}

/// Repeat a string n times.
pub fn exec_repeat(input: &Value, count: i64) -> Result<Value, LimboError> {
    let s = match input {
        Value::Text(t) => t.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    if count <= 0 {
        return Ok(Value::build_text(String::new()));
    }
    let total_len = s
        .len()
        .checked_mul(count as usize)
        .ok_or_else(|| LimboError::InvalidArgument("requested length too large".to_string()))?;
    check_pg_string_length(total_len)?;
    Ok(Value::build_text(s.repeat(count as usize)))
}

/// Simplified to_char: formats a number with the given format pattern.
/// Supports basic PG numeric format patterns (9, 0, S, MI, FM, D, G, PR, TH, L).
pub fn exec_to_char(value: &Value, format: &str) -> Value {
    let num = match value {
        Value::Null => return Value::Null,
        Value::Numeric(_) => value.as_float(),
        Value::Text(t) => match t.as_str().parse::<f64>() {
            Ok(f) => f,
            Err(_) => return Value::Null,
        },
        _ => return Value::Null,
    };

    let result = pg_to_char_numeric(num, format);
    Value::build_text(result)
}

/// pg_input_is_valid(text, type) → boolean
/// Returns true if the text is valid input for the given type.
pub fn exec_pg_input_is_valid(input: &Value, type_name: &str) -> Value {
    let s = match input {
        Value::Text(t) => t.as_str().to_string(),
        Value::Null => return Value::Null,
        v => v.to_string(),
    };
    let valid = crate::pg_catalog::validate_pg_input(&s, type_name).is_none();
    Value::from_i64(if valid { 1 } else { 0 })
}

/// Format a number using PG's to_char numeric format patterns.
fn pg_to_char_numeric(num: f64, format: &str) -> String {
    let is_negative = num < 0.0;
    let abs_num = num.abs();

    // Parse format string for flags
    let upper_fmt = format.to_uppercase();
    let fm = upper_fmt.contains("FM"); // fill mode (suppress padding)
    let has_pr = upper_fmt.contains("PR"); // angle brackets for negative
    let has_s = upper_fmt.contains('S'); // sign
    let has_mi = upper_fmt.starts_with("MI") || upper_fmt.ends_with("MI");

    // Count digit positions
    let mut integer_digits = 0;
    let mut decimal_digits = 0;
    let mut leading_zeros = 0;
    let mut seen_dot = false;

    for ch in upper_fmt.chars() {
        match ch {
            '9' => {
                if seen_dot {
                    decimal_digits += 1;
                } else {
                    integer_digits += 1;
                }
            }
            '0' => {
                if seen_dot {
                    decimal_digits += 1;
                } else {
                    integer_digits += 1;
                    leading_zeros += 1;
                }
            }
            'D' | '.' => seen_dot = true,
            _ => {}
        }
    }

    if integer_digits == 0 && decimal_digits == 0 {
        return format!("{num}");
    }

    // Format the number
    let formatted = if decimal_digits > 0 {
        let prec = decimal_digits;
        format!("{abs_num:.prec$}")
    } else {
        let int_val = abs_num as i64;
        format!("{int_val}")
    };

    // Split into integer and decimal parts
    let parts: Vec<&str> = formatted.split('.').collect();
    let int_part = parts[0];
    let dec_part = if parts.len() > 1 { parts[1] } else { "" };

    // Pad integer part
    let padded_int = if !fm {
        let width = integer_digits.max(int_part.len());
        if leading_zeros > 0 {
            format!("{int_part:0>width$}")
        } else {
            format!("{int_part:>width$}")
        }
    } else {
        int_part.to_string()
    };

    // Build result
    let mut result = if decimal_digits > 0 {
        format!("{padded_int}.{dec_part}")
    } else {
        padded_int
    };

    // Add sign
    if has_pr {
        result = if is_negative {
            format!("<{result}>")
        } else {
            format!(" {result} ")
        };
    } else if has_s {
        let sign_pos = upper_fmt.find('S').unwrap_or(0);
        let sign = if is_negative { "-" } else { "+" };
        if sign_pos == 0 {
            result = format!("{sign}{result}");
        } else {
            result = format!("{result}{sign}");
        }
    } else if has_mi {
        if is_negative {
            result = format!("{result}-");
        } else {
            result = format!("{result} ");
        }
    } else if is_negative {
        result = format!("-{result}");
    } else {
        result = format!(" {result}");
    }

    result
}

/// PostgreSQL GREATEST: variadic max with PG NULL semantics (any NULL arg → NULL).
pub fn exec_greatest<'a, T: Iterator<Item = &'a Value>>(args: T) -> Value {
    Value::exec_max(args)
}

/// PostgreSQL LEAST: variadic min with PG NULL semantics (any NULL arg → NULL).
pub fn exec_least<'a, T: Iterator<Item = &'a Value>>(args: T) -> Value {
    Value::exec_min(args)
}

pub fn exec_rpad(input: &Value, length: usize, fill: &str) -> Result<Value, LimboError> {
    check_pg_string_length(length)?;
    let s = match input {
        Value::Text(t) => t.to_string(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let char_count = s.chars().count();
    if char_count >= length {
        Ok(Value::build_text(
            s.chars().take(length).collect::<String>(),
        ))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            Ok(Value::build_text(s))
        } else {
            let needed = length - char_count;
            let max_char_bytes = fill_chars.iter().map(|c| c.len_utf8()).max().unwrap_or(1);
            let worst_case_pad_bytes = needed.checked_mul(max_char_bytes).ok_or_else(|| {
                LimboError::InvalidArgument("requested length too large".to_string())
            })?;
            check_pg_string_length(worst_case_pad_bytes.saturating_add(s.len()))?;
            let pad: String = fill_chars.iter().cycle().take(needed).collect();
            Ok(Value::build_text(format!("{s}{pad}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_encoding_to_char_unknown_encoding_errors() {
        // real PostgreSQL returns "" for an out-of-range encoding id, never errors.
        assert_eq!(exec_pg_encoding_to_char(9999), Value::build_text(""));
    }

    #[test]
    fn gcd_normal_cases() {
        assert_eq!(exec_gcd(12, 18).unwrap(), Value::from_i64(6));
        assert_eq!(exec_gcd(-12, 18).unwrap(), Value::from_i64(6));
    }

    #[test]
    fn gcd_overflow_raises() {
        assert!(matches!(
            exec_gcd(i64::MIN, 0),
            Err(LimboError::IntegerOverflow)
        ));
        assert!(matches!(
            exec_gcd(0, i64::MIN),
            Err(LimboError::IntegerOverflow)
        ));
        assert!(matches!(
            exec_gcd(i64::MIN, i64::MIN),
            Err(LimboError::IntegerOverflow)
        ));
        // Euclid's algorithm reaches i64::MIN % -1 mid-loop for this pair even
        // though neither input matches the 3 previously special-cased tuples.
        // Before the fix this panics the process instead of returning Err.
        assert!(matches!(
            exec_gcd(i64::MIN, -1),
            Err(LimboError::IntegerOverflow)
        ));
        assert!(matches!(
            exec_gcd(-1, i64::MIN),
            Err(LimboError::IntegerOverflow)
        ));
    }

    #[test]
    fn lcm_normal_cases() {
        assert_eq!(exec_lcm(4, 6).unwrap(), Value::from_i64(12));
        assert_eq!(exec_lcm(0, 5).unwrap(), Value::from_i64(0));
        assert_eq!(exec_lcm(5, 0).unwrap(), Value::from_i64(0));
    }

    #[test]
    fn lcm_overflow_raises() {
        assert!(matches!(
            exec_lcm(i64::MAX, 2),
            Err(LimboError::IntegerOverflow)
        ));
    }

    #[test]
    fn lcm_overflow_raises_on_min_abs() {
        // b.wrapping_abs() previously silently wrapped i64::MIN back to i64::MIN.
        assert!(matches!(
            exec_lcm(i64::MIN, -1),
            Err(LimboError::IntegerOverflow)
        ));
    }

    #[test]
    fn repeat_rejects_oversized_result() {
        let err = exec_repeat(&Value::build_text("x"), 2_000_000_000).unwrap_err();
        assert!(matches!(err, LimboError::InvalidArgument(_)));
    }

    #[test]
    fn lpad_rejects_oversized_length() {
        let err = exec_lpad(&Value::build_text("x"), 2_000_000_000, " ").unwrap_err();
        assert!(matches!(err, LimboError::InvalidArgument(_)));
    }

    #[test]
    fn rpad_rejects_oversized_length() {
        let err = exec_rpad(&Value::build_text("x"), 2_000_000_000, " ").unwrap_err();
        assert!(matches!(err, LimboError::InvalidArgument(_)));
    }

    #[test]
    fn lpad_rejects_oversized_multibyte_fill() {
        // length is under the char-count cap but the multi-byte fill would blow
        // the byte budget if built naively — must still be rejected.
        let err = exec_lpad(&Value::build_text("x"), 1_000_000_000, "\u{1F600}").unwrap_err();
        assert!(matches!(err, LimboError::InvalidArgument(_)));
    }

    #[test]
    fn rpad_rejects_oversized_multibyte_fill() {
        let err = exec_rpad(&Value::build_text("x"), 1_000_000_000, "\u{1F600}").unwrap_err();
        assert!(matches!(err, LimboError::InvalidArgument(_)));
    }

    #[test]
    fn repeat_still_works_under_cap() {
        assert_eq!(
            exec_repeat(&Value::build_text("ab"), 3).unwrap(),
            Value::build_text("ababab")
        );
    }
}
