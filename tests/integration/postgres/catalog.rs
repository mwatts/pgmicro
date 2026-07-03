use crate::common::TempDatabase;
use turso_core::{Numeric, StepResult, Value};

#[turso_macros::test]
fn test_postgres_pg_namespace(db: TempDatabase) {
    let conn = db.connect_limbo();

    // Switch to PostgreSQL dialect
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Query pg_namespace virtual table
    let mut stmt = conn.prepare("SELECT * FROM pg_namespace").unwrap();

    // Should have at least pg_catalog and public namespaces
    let mut found_pg_catalog = false;
    let mut found_public = false;

    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let Value::Text(nspname) = row.get_value(1) {
                    if nspname.as_str() == "pg_catalog" {
                        found_pg_catalog = true;
                    } else if nspname.as_str() == "public" {
                        found_public = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert!(found_pg_catalog, "pg_catalog namespace not found");
    assert!(found_public, "public namespace not found");
}

#[turso_macros::test]
fn test_postgres_pg_class(db: TempDatabase) {
    let conn = db.connect_limbo();

    // Create a test table in SQLite dialect first
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();

    // Switch to PostgreSQL dialect
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Query pg_class virtual table
    let mut stmt = conn
        .prepare("SELECT relname, relkind FROM pg_class WHERE relkind = 'r'")
        .unwrap();

    let mut found_users_table = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let (Value::Text(relname), Value::Text(relkind)) =
                    (row.get_value(0), row.get_value(1))
                {
                    if relname.as_str() == "users" && relkind.as_str() == "r" {
                        found_users_table = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert!(found_users_table, "users table not found in pg_class");
}

#[turso_macros::test]
fn test_postgres_pg_attribute(db: TempDatabase) {
    let conn = db.connect_limbo();

    // Create a test table in SQLite dialect first
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();

    // Switch to PostgreSQL dialect
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Query pg_attribute virtual table
    let mut stmt = conn.prepare("SELECT COUNT(*) FROM pg_attribute").unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Numeric(Numeric::Integer(count)) = row.get_value(0) else {
                panic!("expected integer count");
            };
            assert_eq!(
                *count, 2,
                "pg_attribute should have 2 rows for users(id, name)"
            );
        }
        _ => panic!("Expected row from COUNT query"),
    }
}

#[turso_macros::test]
fn test_postgres_pg_tables(db: TempDatabase) {
    let conn = db.connect_limbo();

    // Create test tables in SQLite dialect first
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER)")
        .unwrap();

    // Switch to PostgreSQL dialect
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Query pg_tables — the standard PG way to list tables
    let mut stmt = conn
        .prepare("SELECT schemaname, tablename FROM pg_tables WHERE schemaname = 'public'")
        .unwrap();

    let mut found_users = false;
    let mut found_orders = false;

    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let Value::Text(schemaname) = row.get_value(0) else {
                    panic!("expected text for schemaname");
                };
                let Value::Text(tablename) = row.get_value(1) else {
                    panic!("expected text for tablename");
                };
                assert_eq!(schemaname.as_str(), "public");
                if tablename.as_str() == "users" {
                    found_users = true;
                } else if tablename.as_str() == "orders" {
                    found_orders = true;
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert!(found_users, "users table not found in pg_tables");
    assert!(found_orders, "orders table not found in pg_tables");
}

#[turso_macros::test]
fn test_postgres_pg_tables_no_internal_tables(db: TempDatabase) {
    let conn = db.connect_limbo();

    // Create a user table
    conn.execute("CREATE TABLE mydata (id INTEGER PRIMARY KEY)")
        .unwrap();

    // Switch to PostgreSQL dialect
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // pg_tables should not expose internal sqlite_* tables
    let mut stmt = conn.prepare("SELECT tablename FROM pg_tables").unwrap();

    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let Value::Text(tablename) = row.get_value(0) else {
                    panic!("expected text for tablename");
                };
                assert!(
                    !tablename.as_str().starts_with("sqlite_"),
                    "internal table {} should not appear in pg_tables",
                    tablename.as_str()
                );
            }
            StepResult::Done => break,
            _ => {}
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// pg_type tests
// ──────────────────────────────────────────────────────────────────────

#[turso_macros::test]
fn test_pg_type_has_builtin_types(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Check well-known types exist with correct OIDs
    let cases = [("int4", 23), ("text", 25), ("bool", 16), ("uuid", 2950)];
    for (type_name, expected_oid) in cases {
        let mut stmt = conn
            .prepare(format!(
                "SELECT oid FROM pg_type WHERE typname = '{type_name}'"
            ))
            .unwrap();
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let Value::Numeric(Numeric::Integer(oid)) = row.get_value(0) else {
                    panic!("expected integer oid for {type_name}");
                };
                assert_eq!(*oid, expected_oid, "wrong OID for {type_name}");
            }
            _ => panic!("{type_name} not found in pg_type"),
        }
    }
}

#[turso_macros::test]
fn test_pg_type_array_types(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // _int4 should exist with typelem pointing to int4 (oid=23)
    let mut stmt = conn
        .prepare("SELECT oid, typelem FROM pg_type WHERE typname = '_int4'")
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Numeric(Numeric::Integer(oid)) = row.get_value(0) else {
                panic!("expected integer oid");
            };
            let Value::Numeric(Numeric::Integer(typelem)) = row.get_value(1) else {
                panic!("expected integer typelem");
            };
            assert_eq!(*oid, 1007, "_int4 should have oid 1007");
            assert_eq!(*typelem, 23, "_int4 typelem should be 23 (int4)");
        }
        _ => panic!("_int4 not found in pg_type"),
    }

    // _text should exist with typelem pointing to text (oid=25)
    let mut stmt = conn
        .prepare("SELECT typelem FROM pg_type WHERE typname = '_text'")
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Numeric(Numeric::Integer(typelem)) = row.get_value(0) else {
                panic!("expected integer typelem");
            };
            assert_eq!(*typelem, 25, "_text typelem should be 25 (text)");
        }
        _ => panic!("_text not found in pg_type"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// pg_index tests
// ──────────────────────────────────────────────────────────────────────

#[turso_macros::test]
fn test_pg_index_populated(db: TempDatabase) {
    let conn = db.connect_limbo();

    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("CREATE INDEX idx_items_name ON items(name)")
        .unwrap();

    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Join pg_index with pg_class to get index name
    let mut stmt = conn
        .prepare(
            "SELECT c.relname, i.indkey, i.indisunique, i.indisprimary
             FROM pg_index i
             JOIN pg_class c ON c.oid = i.indexrelid
             WHERE c.relname = 'idx_items_name'",
        )
        .unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Text(relname) = row.get_value(0) else {
                panic!("expected text relname");
            };
            let Value::Text(indkey) = row.get_value(1) else {
                panic!("expected text indkey");
            };
            let Value::Numeric(Numeric::Integer(indisunique)) = row.get_value(2) else {
                panic!("expected integer indisunique");
            };
            assert_eq!(relname.as_str(), "idx_items_name");
            // name is column 2 (1-based), so indkey should be "2"
            assert_eq!(
                indkey.as_str(),
                "2",
                "indkey should be 2 (name is 2nd column)"
            );
            assert_eq!(*indisunique, 0, "non-unique index");
        }
        _ => panic!("idx_items_name not found in pg_index join pg_class"),
    }
}

#[turso_macros::test]
fn test_pg_index_primary_key(db: TempDatabase) {
    let conn = db.connect_limbo();

    conn.execute("CREATE TABLE pk_test (a TEXT, b TEXT, PRIMARY KEY (a, b))")
        .unwrap();

    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT i.indisprimary, i.indisunique, i.indkey
             FROM pg_index i
             JOIN pg_class ct ON ct.oid = i.indrelid
             WHERE ct.relname = 'pk_test' AND i.indisprimary = 1",
        )
        .unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Numeric(Numeric::Integer(indisprimary)) = row.get_value(0) else {
                panic!("expected integer");
            };
            let Value::Numeric(Numeric::Integer(indisunique)) = row.get_value(1) else {
                panic!("expected integer");
            };
            let Value::Text(indkey) = row.get_value(2) else {
                panic!("expected text indkey");
            };
            assert_eq!(*indisprimary, 1);
            assert_eq!(*indisunique, 1);
            assert_eq!(indkey.as_str(), "1 2", "PK columns a=1, b=2");
        }
        _ => panic!("primary key index not found for pk_test"),
    }
}

#[turso_macros::test]
fn test_pg_index_indisprimary_distinguishes_pk_from_unique(db: TempDatabase) {
    let conn = db.connect_limbo();

    // id must NOT be a rowid-alias PK (i.e. not a bare `INTEGER PRIMARY KEY`),
    // otherwise SQLite backs the PK with the rowid itself instead of a
    // separate sqlite_autoindex_*, and the bug (indisprimary misclassifying
    // the UNIQUE email autoindex) can't be observed via a second PK row.
    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();

    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT indisprimary, indisunique FROM pg_index
             JOIN pg_class c ON c.oid = indrelid
             WHERE c.relname = 't' ORDER BY indisprimary DESC",
        )
        .unwrap();

    let mut pk_count = 0;
    let mut rows = 0;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                rows += 1;
                if stmt.row().unwrap().get_value(0).as_int() == Some(1) {
                    pk_count += 1;
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert_eq!(rows, 2, "expected 2 auto-indexes (pk + unique email)");
    assert_eq!(
        pk_count, 1,
        "exactly one index should be marked indisprimary"
    );
}

// ──────────────────────────────────────────────────────────────────────
// pg_constraint tests
// ──────────────────────────────────────────────────────────────────────

#[turso_macros::test]
fn test_pg_constraint_pk_and_fk(db: TempDatabase) {
    let conn = db.connect_limbo();

    conn.execute("PRAGMA foreign_keys = ON").unwrap();
    conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute(
        "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE)",
    )
    .unwrap();

    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Check PK constraint on parent
    let mut stmt = conn
        .prepare(
            "SELECT conname, contype FROM pg_constraint
             JOIN pg_class c ON c.oid = conrelid
             WHERE c.relname = 'parent' AND contype = 'p'",
        )
        .unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Text(conname) = row.get_value(0) else {
                panic!("expected text conname");
            };
            let Value::Text(contype) = row.get_value(1) else {
                panic!("expected text contype");
            };
            assert_eq!(conname.as_str(), "parent_pkey");
            assert_eq!(contype.as_str(), "p");
        }
        _ => panic!("PK constraint not found for parent table"),
    }

    // Check FK constraint on child
    let mut stmt = conn
        .prepare(
            "SELECT conname, contype, confdeltype FROM pg_constraint
             JOIN pg_class c ON c.oid = conrelid
             WHERE c.relname = 'child' AND contype = 'f'",
        )
        .unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Text(conname) = row.get_value(0) else {
                panic!("expected text conname");
            };
            let Value::Text(contype) = row.get_value(1) else {
                panic!("expected text contype");
            };
            let Value::Text(confdeltype) = row.get_value(2) else {
                panic!("expected text confdeltype");
            };
            assert!(
                conname.as_str().contains("fkey"),
                "FK name should contain 'fkey'"
            );
            assert_eq!(contype.as_str(), "f");
            assert_eq!(confdeltype.as_str(), "c", "ON DELETE CASCADE = 'c'");
        }
        _ => panic!("FK constraint not found for child table"),
    }
}

#[turso_macros::test]
fn test_pg_constraint_check(db: TempDatabase) {
    let conn = db.connect_limbo();

    conn.execute("CREATE TABLE checked (id INTEGER, val INTEGER CHECK(val > 0))")
        .unwrap();

    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT contype, conbin FROM pg_constraint
             JOIN pg_class c ON c.oid = conrelid
             WHERE c.relname = 'checked' AND contype = 'c'",
        )
        .unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Text(contype) = row.get_value(0) else {
                panic!("expected text contype");
            };
            assert_eq!(contype.as_str(), "c", "should be CHECK constraint");
            // conbin should contain the check expression
            if let Value::Text(conbin) = row.get_value(1) {
                assert!(
                    conbin.as_str().contains("val") && conbin.as_str().contains("0"),
                    "conbin should reference val and 0, got: {}",
                    conbin.as_str()
                );
            }
        }
        _ => panic!("CHECK constraint not found for checked table"),
    }
}

#[turso_macros::test]
fn test_pg_constraint_conindid_scoped_to_own_table(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();
    conn.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT con.conindid, idx.indrelid = con.conrelid AS same_table
             FROM pg_constraint con
             JOIN pg_index idx ON idx.indexrelid = con.conindid
             JOIN pg_class c ON c.oid = con.conrelid
             WHERE c.relname = 'users' AND con.contype = 'u'",
        )
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let same_table = stmt.row().unwrap().get_value(1).as_int();
            assert_eq!(
                same_table,
                Some(1),
                "conindid must reference an index on the constraint's own table"
            );
        }
        _ => panic!("UNIQUE constraint on users.email not found"),
    }
}

// ──────────────────────────────────────────────────────────────────────
// pg_class index row tests
// ──────────────────────────────────────────────────────────────────────

#[turso_macros::test]
fn test_pg_class_includes_indexes(db: TempDatabase) {
    let conn = db.connect_limbo();

    conn.execute("CREATE TABLE indexed (id INTEGER PRIMARY KEY, data TEXT)")
        .unwrap();
    conn.execute("CREATE INDEX idx_indexed_data ON indexed(data)")
        .unwrap();

    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // pg_class should have relkind='i' rows for indexes
    let mut stmt = conn
        .prepare(
            "SELECT relname, relkind, relam FROM pg_class
             WHERE relname = 'idx_indexed_data' AND relkind = 'i'",
        )
        .unwrap();

    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Text(relname) = row.get_value(0) else {
                panic!("expected text");
            };
            let Value::Text(relkind) = row.get_value(1) else {
                panic!("expected text");
            };
            let Value::Numeric(Numeric::Integer(relam)) = row.get_value(2) else {
                panic!("expected integer relam");
            };
            assert_eq!(relname.as_str(), "idx_indexed_data");
            assert_eq!(relkind.as_str(), "i");
            assert_eq!(*relam, 403, "index relam should be 403 (btree)");
        }
        _ => panic!("idx_indexed_data not found in pg_class with relkind='i'"),
    }
}

/// Test that schema-qualified pg_catalog references work (e.g. `pg_catalog.pg_class`).
/// psql's `\dt` command sends queries like:
///   SELECT ... FROM pg_catalog.pg_class c
///     LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
///   WHERE ... AND pg_catalog.pg_table_is_visible(c.oid)
/// This must not fail with "no such database: pg_catalog".
#[turso_macros::test]
fn test_pg_catalog_schema_qualified_tables(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE widgets (id INTEGER PRIMARY KEY, label TEXT)")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // pg_catalog.pg_class — the core of psql \dt
    let mut stmt = conn
        .prepare("SELECT c.relname FROM pg_catalog.pg_class c WHERE c.relkind = 'r'")
        .unwrap();
    let mut found = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let Value::Text(name) = row.get_value(0) {
                    if name.as_str() == "widgets" {
                        found = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(found, "widgets table not found via pg_catalog.pg_class");
    drop(stmt);

    // pg_catalog.pg_namespace
    let mut stmt = conn
        .prepare("SELECT nspname FROM pg_catalog.pg_namespace")
        .unwrap();
    let mut found_public = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let Value::Text(ns) = row.get_value(0) {
                    if ns.as_str() == "public" {
                        found_public = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(
        found_public,
        "public namespace not found via pg_catalog.pg_namespace"
    );
    drop(stmt);

    // JOIN across schema-qualified catalog tables (simplified \dt query)
    let mut stmt = conn
        .prepare(
            "SELECT n.nspname, c.relname \
             FROM pg_catalog.pg_class c \
             LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r'",
        )
        .unwrap();
    let mut found = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let Value::Text(name) = row.get_value(1) {
                    if name.as_str() == "widgets" {
                        found = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(
        found,
        "widgets not found via pg_catalog.pg_class JOIN pg_catalog.pg_namespace"
    );
}

/// Test that `public.tablename` also resolves correctly (not as an ATTACH db).
#[turso_macros::test]
fn test_public_schema_qualified_tables(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE gadgets (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("INSERT INTO gadgets (id, name) VALUES (1, 'phone')")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare("SELECT name FROM public.gadgets WHERE id = 1")
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).to_string(), "phone");
        }
        _ => panic!("expected row from public.gadgets"),
    }
}

#[turso_macros::test]
fn test_format_type_expanded(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Basic types
    let cases = vec![
        (16, "boolean"),
        (23, "integer"),
        (25, "text"),
        (114, "json"),
        (3802, "jsonb"),
        (2950, "uuid"),
        (1082, "date"),
        (1114, "timestamp without time zone"),
        (1184, "timestamp with time zone"),
        (1186, "interval"),
        (790, "money"),
        (2278, "void"),
        (2205, "regclass"),
        (2206, "regtype"),
        (1000, "boolean[]"),
        (1007, "integer[]"),
        (1009, "text[]"),
    ];

    for (oid, expected) in cases {
        let mut stmt = conn
            .prepare(format!("SELECT format_type({oid}, -1)"))
            .unwrap();
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                assert_eq!(
                    row.get_value(0).to_string(),
                    expected,
                    "format_type({oid}, -1) should return '{expected}'"
                );
            }
            _ => panic!("expected row for format_type({oid}, -1)"),
        }
    }

    // varchar with typemod
    let mut stmt = conn.prepare("SELECT format_type(1043, 54)").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).to_string(), "character varying(50)");
        }
        _ => panic!("expected row for format_type with typemod"),
    }
}

#[turso_macros::test]
fn test_pg_type_is_visible(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn.prepare("SELECT pg_type_is_visible(23)").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(*row.get_value(0), Value::from_i64(1));
        }
        _ => panic!("expected row"),
    }
}

#[turso_macros::test]
fn test_lpad_rpad(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // lpad with fill char
    let mut stmt = conn.prepare("SELECT lpad('hi', 5, '*')").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).to_string(), "***hi");
        }
        _ => panic!("expected row"),
    }
    drop(stmt);

    // rpad with fill char
    let mut stmt = conn.prepare("SELECT rpad('hi', 5, '-')").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).to_string(), "hi---");
        }
        _ => panic!("expected row"),
    }
    drop(stmt);

    // lpad with default space fill
    let mut stmt = conn.prepare("SELECT lpad('hi', 5)").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).to_string(), "   hi");
        }
        _ => panic!("expected row"),
    }
    drop(stmt);

    // Truncation when string is longer than length
    let mut stmt = conn.prepare("SELECT lpad('hello world', 5)").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).to_string(), "hello");
        }
        _ => panic!("expected row"),
    }
}

#[turso_macros::test]
fn test_pg_get_constraintdef(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT UNIQUE)")
        .unwrap();
    conn.execute("CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE, age INTEGER CHECK(age > 0))")
        .unwrap();

    // Collect all constraint definitions
    let mut stmt = conn
        .prepare("SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint")
        .unwrap();
    let mut defs: Vec<(String, String)> = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let name = row.get_value(0).to_string();
                let def = row.get_value(1).to_string();
                defs.push((name, def));
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    // Check we got real definitions, not NULLs
    assert!(
        !defs.is_empty(),
        "expected constraint definitions, got: {defs:?}"
    );
    for (name, def) in &defs {
        assert!(
            !def.is_empty(),
            "constraint '{name}' should have a definition"
        );
    }

    // Find a PK constraint
    let has_pk = defs.iter().any(|(_, d)| d.starts_with("PRIMARY KEY"));
    assert!(
        has_pk,
        "should have a PRIMARY KEY constraint, got: {defs:?}"
    );

    // Find the FK constraint
    let has_fk = defs
        .iter()
        .any(|(_, d)| d.contains("FOREIGN KEY") && d.contains("REFERENCES"));
    assert!(
        has_fk,
        "should have a FOREIGN KEY constraint, got: {defs:?}"
    );
}

#[turso_macros::test]
fn test_pg_get_indexdef(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price REAL)")
        .unwrap();
    conn.execute("CREATE INDEX idx_items_name ON items(name)")
        .unwrap();
    conn.execute("CREATE UNIQUE INDEX idx_items_price ON items(price)")
        .unwrap();

    // Get index definitions via pg_class (indexes have relkind='i')
    let mut stmt = conn
        .prepare("SELECT pg_get_indexdef(oid) FROM pg_class WHERE relkind = 'i'")
        .unwrap();
    let mut defs: Vec<String> = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let def = row.get_value(0).to_string();
                defs.push(def);
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert!(!defs.is_empty(), "expected index definitions");

    let has_name_idx = defs
        .iter()
        .any(|d| d.contains("idx_items_name") && d.contains("items") && d.contains("name"));
    assert!(has_name_idx, "should have idx_items_name definition");

    let has_unique_idx = defs
        .iter()
        .any(|d| d.contains("UNIQUE") && d.contains("idx_items_price"));
    assert!(
        has_unique_idx,
        "should have UNIQUE idx_items_price definition"
    );
}

#[turso_macros::test]
fn test_pg_attrdef_populated(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("CREATE TABLE defaults_test (id INTEGER PRIMARY KEY, name TEXT DEFAULT 'unnamed', score INTEGER DEFAULT 0)")
        .unwrap();

    let mut stmt = conn.prepare("SELECT adnum, adbin FROM pg_attrdef").unwrap();
    let mut rows: Vec<(i64, String)> = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let adnum = match row.get_value(0) {
                    Value::Numeric(Numeric::Integer(n)) => *n,
                    _ => panic!("expected integer adnum"),
                };
                let adbin = row.get_value(1).to_string();
                rows.push((adnum, adbin));
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert!(
        rows.len() >= 2,
        "expected at least 2 default values, got {}",
        rows.len()
    );
}

// ---------------------------------------------------------------------------
// Tests for tables created in PG mode (not SQLite mode).
// These catch regressions where PG CREATE TABLE compiles but the bytecode
// is never executed (e.g. when DDL statements with 0 result columns are
// not stepped through).
// ---------------------------------------------------------------------------

#[turso_macros::test]
fn test_pg_create_table_visible_in_pg_tables(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // Create table purely in PG mode
    conn.execute("CREATE TABLE items (id INT PRIMARY KEY, name TEXT)")
        .unwrap();

    // Verify it appears in pg_tables
    let mut stmt = conn
        .prepare("SELECT tablename FROM pg_tables WHERE schemaname = 'public'")
        .unwrap();
    let mut found = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let Value::Text(name) = row.get_value(0) {
                    if name.as_str() == "items" {
                        found = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(found, "table created in PG mode not found in pg_tables");
}

#[turso_macros::test]
fn test_pg_create_table_visible_in_pg_class(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE TABLE widgets (id INT, label TEXT)")
        .unwrap();

    let mut stmt = conn
        .prepare("SELECT relname FROM pg_class WHERE relkind = 'r'")
        .unwrap();
    let mut found = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let Value::Text(name) = row.get_value(0) {
                    if name.as_str() == "widgets" {
                        found = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(found, "table created in PG mode not found in pg_class");
}

#[turso_macros::test]
fn test_pg_attribute_atttypmod(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE TABLE typmod_test (name varchar(100), amount numeric(10,2))")
        .unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT a.attname, a.atttypmod \
             FROM pg_attribute a \
             JOIN pg_class c ON a.attrelid = c.oid \
             WHERE c.relname = 'typmod_test' AND a.attnum > 0 AND a.attisdropped = 0 \
             ORDER BY a.attnum",
        )
        .unwrap();

    let mut columns = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let name = row.get_value(0).to_string();
                let typmod = match row.get_value(1) {
                    Value::Numeric(Numeric::Integer(n)) => *n,
                    other => panic!("expected integer atttypmod, got {other:?}"),
                };
                columns.push((name, typmod));
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert_eq!(columns.len(), 2, "expected 2 columns, got {columns:?}");
    assert_eq!(columns[0].0, "name");
    assert_eq!(columns[0].1, 104, "varchar(100) -> atttypmod 104");
    assert_eq!(columns[1].0, "amount");
    assert_eq!(columns[1].1, 655366, "numeric(10,2) -> atttypmod 655366");
}

#[turso_macros::test]
fn test_pg_attribute_varlena_columns_not_byval(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT attlen, attbyval, attalign FROM pg_attribute
             JOIN pg_class c ON c.oid = attrelid
             WHERE c.relname = 't' AND attname = 'name'",
        )
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(
                row.get_value(0).as_int(),
                Some(-1),
                "text is varlena, attlen must be -1"
            );
            assert_eq!(
                row.get_value(1).as_int(),
                Some(0),
                "text is pass-by-reference, attbyval must be false"
            );
        }
        _ => panic!("column 'name' not found in pg_attribute"),
    }
}

#[turso_macros::test]
fn test_pg_create_table_columns_in_pg_attribute(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE TABLE products (id INT PRIMARY KEY, name TEXT, price INT)")
        .unwrap();

    // Join pg_attribute + pg_class + pg_type to get column info (same query pgmicro \d uses)
    let mut stmt = conn
        .prepare(
            "SELECT a.attname, t.typname \
             FROM pg_attribute a \
             JOIN pg_class c ON a.attrelid = c.oid \
             JOIN pg_type t ON a.atttypid = t.oid \
             WHERE c.relname = 'products' AND a.attnum > 0 AND a.attisdropped = 0 \
             ORDER BY a.attnum",
        )
        .unwrap();

    let mut columns = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let name = row.get_value(0).to_string();
                let typ = row.get_value(1).to_string();
                columns.push((name, typ));
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert_eq!(columns.len(), 3, "expected 3 columns, got {columns:?}");
    assert_eq!(columns[0].0, "id");
    assert_eq!(columns[1].0, "name");
    assert_eq!(columns[1].1, "text");
    assert_eq!(columns[2].0, "price");
}

#[turso_macros::test]
fn test_pg_create_table_then_insert_and_select(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE TABLE kv (k TEXT, v INT)").unwrap();
    conn.execute("INSERT INTO kv VALUES ('hello', 42)").unwrap();

    let mut stmt = conn.prepare("SELECT k, v FROM kv").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Text(k) = row.get_value(0) else {
                panic!("expected text");
            };
            assert_eq!(k.as_str(), "hello");
            let Value::Numeric(Numeric::Integer(v)) = row.get_value(1) else {
                panic!("expected integer");
            };
            assert_eq!(*v, 42);
        }
        _ => panic!("expected a row"),
    }
    assert!(
        matches!(stmt.step().unwrap(), StepResult::Done),
        "expected exactly one row"
    );
}

#[turso_macros::test]
fn test_pg_create_table_in_pg_database(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE SCHEMA foo").unwrap();

    let main_db_name = db
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("temp database path should have a file stem");

    let mut stmt = conn
        .prepare("SELECT datname FROM pg_database ORDER BY datname")
        .unwrap();
    let mut datnames = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let Value::Text(datname) = row.get_value(0) else {
                    panic!("expected text datname");
                };
                datnames.push(datname.as_str().to_string());
            }
            StepResult::Done => break,
            _ => {}
        }
    }

    assert!(
        datnames.contains(&main_db_name.to_string()),
        "pg_database should include main database '{main_db_name}', got {datnames:?}"
    );
    assert!(
        datnames.contains(&"foo".to_string()),
        "pg_database should include attached schema 'foo', got {datnames:?}"
    );
}

#[turso_macros::test]
fn test_pg_authid_and_pg_user_stubs(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare("SELECT rolname, rolsuper FROM pg_authid")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected pg_authid row");
    };
    let row = stmt.row().unwrap();
    let Value::Text(rolname) = row.get_value(0) else {
        panic!("expected rolname text");
    };
    let Value::Numeric(Numeric::Integer(superuser)) = row.get_value(1) else {
        panic!("expected rolsuper integer");
    };
    assert_eq!(rolname.as_str(), "turso");
    assert_eq!(*superuser, 1);

    let mut stmt = conn
        .prepare("SELECT usename, usesuper FROM pg_user")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected pg_user row");
    };
    let row = stmt.row().unwrap();
    let Value::Text(usename) = row.get_value(0) else {
        panic!("expected usename text");
    };
    let Value::Numeric(Numeric::Integer(superuser)) = row.get_value(1) else {
        panic!("expected usesuper integer");
    };
    assert_eq!(usename.as_str(), "turso");
    assert_eq!(*superuser, 1);
}

#[turso_macros::test]
fn test_pg_enum_labels(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE TYPE mood AS ENUM ('happy', 'sad')")
        .unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT enumlabel FROM pg_enum e \
             JOIN pg_type t ON e.enumtypid = t.oid \
             WHERE t.typname = 'mood' ORDER BY e.enumsortorder",
        )
        .unwrap();
    let mut labels = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let Value::Text(label) = row.get_value(0) else {
                    panic!("expected enum label text");
                };
                labels.push(label.as_str().to_string());
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert_eq!(labels, vec!["happy", "sad"]);
}

#[turso_macros::test]
fn test_pg_create_and_drop_role(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("CREATE ROLE appuser").unwrap();

    let mut stmt = conn
        .prepare("SELECT rolname, rolcanlogin FROM pg_roles WHERE rolname = 'appuser'")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected appuser in pg_roles");
    };
    let row = stmt.row().unwrap();
    let Value::Text(rolname) = row.get_value(0) else {
        panic!("expected rolname text");
    };
    let Value::Numeric(Numeric::Integer(canlogin)) = row.get_value(1) else {
        panic!("expected rolcanlogin integer");
    };
    assert_eq!(rolname.as_str(), "appuser");
    assert_eq!(*canlogin, 1);

    let mut stmt = conn.prepare("SELECT pg_get_userbyid(10)").unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected pg_get_userbyid row");
    };
    let row = stmt.row().unwrap();
    let Value::Text(name) = row.get_value(0) else {
        panic!("expected username text");
    };
    assert_eq!(name.as_str(), "turso");

    conn.execute("DROP ROLE appuser").unwrap();

    let mut stmt = conn
        .prepare("SELECT COUNT(*) FROM pg_roles WHERE rolname = 'appuser'")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected count row");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(count)) = row.get_value(0) else {
        panic!("expected count integer");
    };
    assert_eq!(*count, 0);
}

#[turso_macros::test]
fn test_pg_proc_stable_oids(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare("SELECT oid, proname FROM pg_proc WHERE proname = 'abs'")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected abs in pg_proc");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(oid)) = row.get_value(0) else {
        panic!("expected integer oid");
    };
    let Value::Text(proname) = row.get_value(1) else {
        panic!("expected proname text");
    };
    assert_eq!(proname.as_str(), "abs");
    assert!(
        *oid >= 80_000,
        "pg_proc OID should be stable base range, got {oid}"
    );

    let first_oid = *oid;
    let mut stmt = conn
        .prepare("SELECT oid FROM pg_proc WHERE proname = 'abs'")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected abs oid on second scan");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(oid)) = row.get_value(0) else {
        panic!("expected integer oid");
    };
    assert_eq!(*oid, first_oid, "pg_proc OID should be stable across scans");
}

#[turso_macros::test]
fn test_pg_class_oid_index_filter(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE oid_filter_test (id INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare("SELECT oid FROM pg_class WHERE relname = 'oid_filter_test'")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected table oid row");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(table_oid)) = row.get_value(0) else {
        panic!("expected integer table oid");
    };

    let sql = format!("SELECT relname FROM pg_class WHERE oid = {table_oid}");
    let mut stmt = conn.prepare(&sql).unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected row for oid filter");
    };
    let row = stmt.row().unwrap();
    let Value::Text(relname) = row.get_value(0) else {
        panic!("expected relname text");
    };
    assert_eq!(relname.as_str(), "oid_filter_test");
}

#[turso_macros::test]
fn test_pg_comment_on_table(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE comment_me (id INTEGER PRIMARY KEY)")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("COMMENT ON TABLE comment_me IS 'hello table'")
        .unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT d.description FROM pg_description d \
             JOIN pg_class c ON c.oid = d.objoid \
             WHERE c.relname = 'comment_me' AND d.objsubid = 0",
        )
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected comment row");
    };
    let row = stmt.row().unwrap();
    let Value::Text(desc) = row.get_value(0) else {
        panic!("expected description text");
    };
    assert_eq!(desc.as_str(), "hello table");
}

#[turso_macros::test]
fn test_pg_prepare_execute_deallocate(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    conn.execute("PREPARE sel AS SELECT 7 + $1").unwrap();

    let mut stmt = conn.prepare("EXECUTE sel(5)").unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected execute row");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(v)) = row.get_value(0) else {
        panic!("expected integer result");
    };
    assert_eq!(*v, 12);

    conn.execute("DEALLOCATE sel").unwrap();
    assert!(conn.prepare("EXECUTE sel(1)").is_err());
}

#[turso_macros::test]
fn test_pg_proc_alias_names(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    for alias in ["char_length", "btrim", "strpos"] {
        let sql = format!("SELECT proname FROM pg_proc WHERE proname = '{alias}'");
        let mut stmt = conn.prepare(&sql).unwrap();
        let StepResult::Row = stmt.step().unwrap() else {
            panic!("expected pg_proc row for {alias}");
        };
        let row = stmt.row().unwrap();
        let Value::Text(name) = row.get_value(0) else {
            panic!("expected proname text");
        };
        assert_eq!(name.as_str(), alias);
    }
}

#[turso_macros::test]
fn test_pg_proc_builtin_functions_in_pg_catalog_namespace(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare("SELECT pronamespace FROM pg_proc WHERE proname = 'lower'")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected lower() in pg_proc");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(ns)) = row.get_value(0) else {
        panic!("expected integer pronamespace");
    };
    assert_eq!(
        *ns, 11,
        "built-in function lower() should be in pg_catalog (oid 11), not public"
    );
}

#[turso_macros::test]
fn test_pg_proc_aggregate_prokind_is_a(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn
        .prepare("SELECT prokind FROM pg_proc WHERE proname = 'sum'")
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let Value::Text(prokind) = stmt.row().unwrap().get_value(0) else {
                panic!("expected text prokind")
            };
            assert_eq!(
                prokind.as_str(),
                "a",
                "sum() is an aggregate, must report prokind='a'"
            );
        }
        _ => panic!("sum() not found in pg_proc"),
    }
}

#[turso_macros::test]
fn test_pg_proc_window_function_prokind_is_w(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn
        .prepare("SELECT prokind FROM pg_proc WHERE proname = 'row_number'")
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let Value::Text(prokind) = stmt.row().unwrap().get_value(0) else {
                panic!("expected text prokind")
            };
            assert_eq!(
                prokind.as_str(),
                "w",
                "row_number() is a genuine window function, must report prokind='w'"
            );
        }
        _ => panic!("row_number() not found in pg_proc"),
    }
}

#[turso_macros::test]
fn test_pg_collation_builtin_rows(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare("SELECT collname FROM pg_collation ORDER BY oid")
        .unwrap();
    let mut names = Vec::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                let Value::Text(name) = row.get_value(0) else {
                    panic!("expected collname text");
                };
                names.push(name.as_str().to_string());
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert_eq!(names, vec!["default", "ucs_basic", "C", "POSIX"]);
}

#[turso_macros::test]
fn test_pg_bare_numeric_type_oid(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("CREATE TABLE num_test (amount NUMERIC)")
        .unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT t.typname, a.atttypid \
             FROM pg_attribute a \
             JOIN pg_class c ON a.attrelid = c.oid \
             JOIN pg_type t ON a.atttypid = t.oid \
             WHERE c.relname = 'num_test' AND a.attname = 'amount'",
        )
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected numeric column metadata");
    };
    let row = stmt.row().unwrap();
    let Value::Text(typname) = row.get_value(0) else {
        panic!("expected typname");
    };
    let Value::Numeric(Numeric::Integer(oid)) = row.get_value(1) else {
        panic!("expected atttypid");
    };
    assert_eq!(typname.as_str(), "numeric");
    assert_eq!(*oid, 1700);
}

#[turso_macros::test]
fn test_pg_listen_notify_delivery(db: TempDatabase) {
    let listener = db.connect_limbo();
    let notifier = db.connect_limbo();
    listener.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    notifier.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    listener.execute("LISTEN alerts").unwrap();
    notifier.execute("NOTIFY alerts, 'hello'").unwrap();

    let received = listener.drain_pg_notifications();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].channel, "alerts");
    assert_eq!(received[0].payload, "hello");

    listener.execute("UNLISTEN alerts").unwrap();
    listener.execute("UNLISTEN *").unwrap();
}

#[turso_macros::test]
fn test_pg_listen_notify_self_delivery(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("LISTEN alerts").unwrap();
    conn.execute("NOTIFY alerts, 'ping'").unwrap();

    let received = conn.drain_pg_notifications();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].payload, "ping");
}

#[turso_macros::test]
fn test_pg_attribute_excludes_hidden_columns(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname = 't'",
        )
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let count = stmt.row().unwrap().get_value(0).as_int().unwrap();
            assert_eq!(
                count, 2,
                "pg_attribute row count must match visible column count only"
            );
        }
        _ => panic!("count query failed"),
    }
}

/// Regression test for H29: `ALTER TABLE ... ADD COLUMN` builds its new
/// `Column` via `impl TryFrom<&ColumnDefinition> for Column`
/// (`core/schema.rs:4659`), which sets `hidden = ty_str.contains("HIDDEN")`.
/// The type-name parser (`parser/src/parser.rs::parse_type`) concatenates
/// consecutive identifier tokens, so `extra TEXT HIDDEN` parses as a single
/// type name `"TEXT HIDDEN"`, so an `ALTER TABLE ADD COLUMN extra TEXT HIDDEN`
/// genuinely sets `Column::hidden() == true` on an ordinary BTree table (no
/// virtual table or extension required). Verified directly via
/// `PRAGMA table_xinfo(t)`, whose `hidden` output column is 1 for `extra`.
///
/// Note: plain `CREATE TABLE` cannot reproduce this — its column-building
/// path (`core/schema.rs`, around line 4035) hardcodes `hidden: false`
/// regardless of the type text. Only the `ALTER TABLE ADD COLUMN` /
/// `Column::try_from(&ColumnDefinition)` path can set it, which is why
/// this test uses `ALTER TABLE` instead of a single `CREATE TABLE`.
///
/// Before the fix, `pg_attribute`/`pg_class.relnatts` counted the hidden
/// column too, so `t` (2 visible + 1 hidden = 3 declared columns) reported 3
/// attributes/relnatts instead of the correct 2. This exercises the
/// `col.hidden()` branch that the plain 2-column regression test above
/// cannot reach — confirmed by temporarily reverting the fix and observing
/// this test fail with `left: 3, right: 2` before reapplying it.
#[turso_macros::test]
fn test_pg_attribute_and_relnatts_exclude_hidden_column(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("ALTER TABLE t ADD COLUMN extra TEXT HIDDEN")
        .unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn
        .prepare(
            "SELECT COUNT(*) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname = 't'",
        )
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let count = stmt.row().unwrap().get_value(0).as_int().unwrap();
            assert_eq!(
                count, 2,
                "pg_attribute must exclude the hidden column, leaving only 2 visible columns"
            );
        }
        _ => panic!("count query failed"),
    }

    let mut stmt = conn
        .prepare("SELECT relnatts FROM pg_class WHERE relname = 't'")
        .unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let relnatts = stmt.row().unwrap().get_value(0).as_int().unwrap();
            assert_eq!(
                relnatts, 2,
                "relnatts must exclude the hidden column, leaving only 2 visible columns"
            );
        }
        _ => panic!("relnatts query failed"),
    }
}
