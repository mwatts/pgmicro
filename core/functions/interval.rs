use crate::interval::Interval;
use crate::types::Value;
use crate::LimboError;

pub fn exec_interval_in(input: &Value) -> Result<Value, LimboError> {
    match input {
        Value::Null => Ok(Value::Null),
        Value::Text(t) => {
            let iv = Interval::from_text(t.as_str())?;
            Ok(Value::from_blob(iv.to_blob().to_vec()))
        }
        other => Err(LimboError::Constraint(format!(
            "invalid input for type interval: \"{other}\""
        ))),
    }
}

pub fn exec_interval_out(input: &Value) -> Result<Value, LimboError> {
    match input {
        Value::Null => Ok(Value::Null),
        Value::Blob(b) => {
            let iv = Interval::from_blob(b)?;
            Ok(Value::build_text(iv.to_text()))
        }
        other => Err(LimboError::Constraint(format!(
            "interval_out: expected blob, got \"{other}\""
        ))),
    }
}

fn interval_from_value(v: &Value) -> Result<Interval, LimboError> {
    match v {
        Value::Null => Err(LimboError::NullValue),
        Value::Blob(b) => Interval::from_blob(b),
        other => Err(LimboError::Constraint(format!(
            "expected interval blob, got \"{other}\""
        ))),
    }
}

pub fn exec_interval_pl(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let sum = interval_from_value(a)?.add(interval_from_value(b)?)?;
    Ok(Value::from_blob(sum.to_blob().to_vec()))
}

pub fn exec_interval_mi(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let diff = interval_from_value(a)?.sub(interval_from_value(b)?)?;
    Ok(Value::from_blob(diff.to_blob().to_vec()))
}

pub fn exec_interval_mul(iv: &Value, factor: &Value) -> Result<Value, LimboError> {
    if matches!(iv, Value::Null) || matches!(factor, Value::Null) {
        return Ok(Value::Null);
    }
    let f = factor.to_float_or_zero();
    let scaled = interval_from_value(iv)?.mul(f)?;
    Ok(Value::from_blob(scaled.to_blob().to_vec()))
}

pub fn exec_interval_div(iv: &Value, divisor: &Value) -> Result<Value, LimboError> {
    if matches!(iv, Value::Null) || matches!(divisor, Value::Null) {
        return Ok(Value::Null);
    }
    let d = divisor.to_float_or_zero();
    let scaled = interval_from_value(iv)?.div(d)?;
    Ok(Value::from_blob(scaled.to_blob().to_vec()))
}

pub fn exec_interval_lt(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let lhs = interval_from_value(a)?;
    let rhs = interval_from_value(b)?;
    Ok(Value::from_i64(i64::from(interval_lt(lhs, rhs))))
}

pub fn exec_interval_eq(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let lhs = interval_from_value(a)?;
    let rhs = interval_from_value(b)?;
    Ok(Value::from_i64(i64::from(lhs == rhs)))
}

fn interval_lt(a: Interval, b: Interval) -> bool {
    (a.months, a.days, a.microseconds) < (b.months, b.days, b.microseconds)
}

pub fn exec_justify_days(iv: &Value) -> Result<Value, LimboError> {
    match iv {
        Value::Null => Ok(Value::Null),
        _ => {
            let out = interval_from_value(iv)?.justify_days();
            Ok(Value::from_blob(out.to_blob().to_vec()))
        }
    }
}

pub fn exec_justify_hours(iv: &Value) -> Result<Value, LimboError> {
    match iv {
        Value::Null => Ok(Value::Null),
        _ => {
            let out = interval_from_value(iv)?.justify_hours();
            Ok(Value::from_blob(out.to_blob().to_vec()))
        }
    }
}

pub fn exec_interval_extract(field: &Value, iv: &Value) -> Result<Value, LimboError> {
    if matches!(field, Value::Null) || matches!(iv, Value::Null) {
        return Ok(Value::Null);
    }
    let field_name = match field {
        Value::Text(t) => t.as_str(),
        other => {
            return Err(LimboError::Constraint(format!(
                "interval_extract: field must be text, got \"{other}\""
            )));
        }
    };
    let value = interval_from_value(iv)?.extract_field(field_name)?;
    Ok(Value::from_f64(value))
}

pub fn exec_timestamp_pl_interval(ts: &Value, iv: &Value) -> Result<Value, LimboError> {
    if matches!(ts, Value::Null) || matches!(iv, Value::Null) {
        return Ok(Value::Null);
    }
    let ts_text = match ts {
        Value::Text(t) => t.as_str(),
        other => {
            return Err(LimboError::Constraint(format!(
                "timestamp_pl_interval: expected text timestamp, got \"{other}\""
            )));
        }
    };
    let blob = match iv {
        Value::Blob(b) => b.as_slice(),
        other => {
            return Err(LimboError::Constraint(format!(
                "timestamp_pl_interval: expected interval blob, got \"{other}\""
            )));
        }
    };
    let out = crate::interval::timestamp_pl_interval(ts_text, blob)?;
    Ok(Value::build_text(out))
}

pub fn exec_timestamp_mi_interval(ts: &Value, iv: &Value) -> Result<Value, LimboError> {
    if matches!(ts, Value::Null) || matches!(iv, Value::Null) {
        return Ok(Value::Null);
    }
    let ts_text = match ts {
        Value::Text(t) => t.as_str(),
        other => {
            return Err(LimboError::Constraint(format!(
                "timestamp_mi_interval: expected text timestamp, got \"{other}\""
            )));
        }
    };
    let blob = match iv {
        Value::Blob(b) => b.as_slice(),
        other => {
            return Err(LimboError::Constraint(format!(
                "timestamp_mi_interval: expected interval blob, got \"{other}\""
            )));
        }
    };
    let out = crate::interval::timestamp_mi_interval(ts_text, blob)?;
    Ok(Value::build_text(out))
}
