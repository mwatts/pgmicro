use crate::common::TempDatabase;
use turso_core::{Numeric, StepResult, Value};

#[turso_macros::test(mvcc)]
fn test_pg_interval_column_create_and_roundtrip(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE TABLE events (id INTEGER, span INTERVAL)")
        .unwrap();
    conn.execute("INSERT INTO events (id, span) VALUES (1, '2 days')")
        .unwrap();
    conn.execute("INSERT INTO events (id, span) VALUES (2, '1 mon')")
        .unwrap();

    let mut rows = conn
        .query("SELECT span FROM events ORDER BY id")
        .unwrap()
        .unwrap();
    for expected in ["2 days", "1 mon"] {
        let StepResult::Row = rows.step().unwrap() else {
            panic!("expected row");
        };
        let row = rows.row().unwrap();
        let Value::Text(v) = row.get_value(0) else {
            panic!("expected decoded interval text, got {:?}", row.get_value(0));
        };
        assert_eq!(v.value, expected);
    }
}

#[turso_macros::test(mvcc)]
fn test_pg_money_column_create_and_roundtrip(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE TABLE prices (id INTEGER, amount MONEY)")
        .unwrap();
    conn.execute("INSERT INTO prices (id, amount) VALUES (1, '$12.34')")
        .unwrap();
    conn.execute("INSERT INTO prices (id, amount) VALUES (2, '($1.00)')")
        .unwrap();

    let mut rows = conn
        .query("SELECT amount FROM prices ORDER BY id")
        .unwrap()
        .unwrap();
    for expected in ["$12.34", "($1.00)"] {
        let StepResult::Row = rows.step().unwrap() else {
            panic!("expected row");
        };
        let row = rows.row().unwrap();
        let Value::Text(v) = row.get_value(0) else {
            panic!("expected decoded money text, got {:?}", row.get_value(0));
        };
        assert_eq!(v.value, expected);
    }
}

#[turso_macros::test(mvcc)]
fn test_pg_interval_literal_and_arithmetic(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    let mut rows = conn
        .query("SELECT interval_out(INTERVAL '1 day')")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected interval text");
    };
    assert_eq!(v.value, "1 day");
    drop(rows);

    let mut rows = conn
        .query("SELECT INTERVAL '1 mon' + INTERVAL '1 mon'")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected interval text");
    };
    assert_eq!(v.value, "2 mons");
}

#[turso_macros::test(mvcc)]
fn test_pg_timestamp_minus_interval(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    let mut rows = conn
        .query("SELECT '2024-01-31 12:00:00'::timestamp - INTERVAL '1 month'")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected timestamp text");
    };
    assert!(
        v.value.starts_with("2023-12-31"),
        "expected calendar month subtraction, got '{}'",
        v.value
    );
}

#[turso_macros::test(mvcc)]
fn test_pg_extract_from_interval(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    let mut rows = conn
        .query("SELECT EXTRACT(day FROM INTERVAL '3 days 4 hours')")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Numeric(Numeric::Float(v)) = row.get_value(0) else {
        panic!("expected float extract result");
    };
    assert_eq!(*v, 3.0);
}

#[turso_macros::test(mvcc)]
fn test_pg_justify_days(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    let mut rows = conn
        .query("SELECT interval_out(justify_days(INTERVAL '1 mon'))")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected interval text");
    };
    assert_eq!(v.value, "30 days");
}

#[turso_macros::test(mvcc)]
fn test_pg_pg_attribute_interval_money_oids(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE TABLE t (iv INTERVAL, m MONEY)")
        .unwrap();

    let mut rows = conn
        .query(
            "SELECT a.attname, ty.typname, ty.oid \
             FROM pg_attribute a \
             JOIN pg_class c ON a.attrelid = c.oid \
             JOIN pg_type ty ON a.atttypid = ty.oid \
             WHERE c.relname = 't' AND a.attnum > 0 AND a.attisdropped = 0 \
             ORDER BY a.attnum",
        )
        .unwrap()
        .unwrap();

    let mut got = Vec::new();
    loop {
        match rows.step().unwrap() {
            StepResult::Row => {
                let row = rows.row().unwrap();
                let name = row.get_value(0).to_string();
                let typname = row.get_value(1).to_string();
                let oid = match row.get_value(2) {
                    Value::Numeric(Numeric::Integer(i)) => *i,
                    other => panic!("expected oid integer, got {other:?}"),
                };
                got.push((name, typname, oid));
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert_eq!(
        got,
        vec![
            ("iv".to_string(), "interval".to_string(), 1186),
            ("m".to_string(), "money".to_string(), 790),
        ]
    );
}

#[turso_macros::test(mvcc)]
fn test_pg_interval_column_arithmetic_and_where(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE TABLE t (iv interval)").unwrap();
    conn.execute("INSERT INTO t VALUES ('1 day'), ('2 days')")
        .unwrap();

    let mut rows = conn
        .query("SELECT iv + iv FROM t ORDER BY iv")
        .unwrap()
        .unwrap();
    let mut sums = Vec::new();
    loop {
        match rows.step().unwrap() {
            StepResult::Row => {
                let row = rows.row().unwrap();
                let Value::Text(v) = row.get_value(0) else {
                    panic!("expected interval text, got {:?}", row.get_value(0));
                };
                sums.push(v.value.clone());
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert_eq!(sums, vec!["2 days", "4 days"]);

    let mut rows = conn
        .query("SELECT iv FROM t WHERE iv = '1 day' ORDER BY iv")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row from WHERE");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected interval text");
    };
    assert_eq!(v.value, "1 day");
    let StepResult::Done = rows.step().unwrap() else {
        panic!("expected single matching row");
    };
}

#[turso_macros::test(mvcc)]
fn test_pg_interval_scale_and_money_arithmetic(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE TABLE t (iv interval, m money)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES ('1 day', '$1.00')")
        .unwrap();

    let mut rows = conn.query("SELECT iv * 2 FROM t").unwrap().unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected interval text");
    };
    assert_eq!(v.value, "2 days");

    let mut rows = conn.query("SELECT m + '$0.50' FROM t").unwrap().unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(v) = row.get_value(0) else {
        panic!("expected money text");
    };
    assert_eq!(v.value, "$1.50");
}
