use crate::money::Money;
use crate::numeric::Numeric;
use crate::types::Value;
use crate::LimboError;

pub fn exec_money_in(input: &Value) -> Result<Value, LimboError> {
    match input {
        Value::Null => Ok(Value::Null),
        Value::Text(t) => {
            let m = Money::from_text(t.as_str())?;
            Ok(Value::from_i64(m.cents()))
        }
        other => Err(LimboError::Constraint(format!(
            "invalid input for type money: \"{other}\""
        ))),
    }
}

pub fn exec_money_out(input: &Value) -> Result<Value, LimboError> {
    match input {
        Value::Null => Ok(Value::Null),
        Value::Numeric(Numeric::Integer(i)) => {
            Ok(Value::build_text(Money::from_cents(*i).to_text()))
        }
        other => Err(LimboError::Constraint(format!(
            "money_out: expected integer, got \"{other}\""
        ))),
    }
}

fn money_from_value(v: &Value) -> Result<Money, LimboError> {
    match v {
        Value::Null => Err(LimboError::NullValue),
        Value::Numeric(Numeric::Integer(i)) => Ok(Money::from_cents(*i)),
        other => Err(LimboError::Constraint(format!(
            "expected money integer, got \"{other}\""
        ))),
    }
}

pub fn exec_money_pl(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let sum = money_from_value(a)?.add(money_from_value(b)?)?;
    Ok(Value::from_i64(sum.cents()))
}

pub fn exec_money_mi(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let diff = money_from_value(a)?.sub(money_from_value(b)?)?;
    Ok(Value::from_i64(diff.cents()))
}

pub fn exec_money_mul(m: &Value, factor: &Value) -> Result<Value, LimboError> {
    if matches!(m, Value::Null) || matches!(factor, Value::Null) {
        return Ok(Value::Null);
    }
    let f = factor.to_float_or_zero();
    let scaled = money_from_value(m)?.mul(f)?;
    Ok(Value::from_i64(scaled.cents()))
}

pub fn exec_money_div(m: &Value, divisor: &Value) -> Result<Value, LimboError> {
    if matches!(m, Value::Null) || matches!(divisor, Value::Null) {
        return Ok(Value::Null);
    }
    let d = divisor.to_float_or_zero();
    let scaled = money_from_value(m)?.div(d)?;
    Ok(Value::from_i64(scaled.cents()))
}

pub fn exec_money_lt(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let lhs = money_from_value(a)?;
    let rhs = money_from_value(b)?;
    Ok(Value::from_i64(i64::from(lhs.cents() < rhs.cents())))
}

pub fn exec_money_eq(a: &Value, b: &Value) -> Result<Value, LimboError> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let lhs = money_from_value(a)?;
    let rhs = money_from_value(b)?;
    Ok(Value::from_i64(i64::from(lhs == rhs)))
}
