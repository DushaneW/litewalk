//! info, tables, indexes, objects, schema.

use std::fmt::Write;

use litewalk::{AutoVacuum, ColumnDefault, Generated, ObjectKind, Table, Value};

use super::json::Json;
use super::{emit, extra_args, open, path_arg, wal_path, Args, CliResult, Out};

pub(super) fn version_string(n: u32) -> String {
    format!("{}.{}.{}", n / 1_000_000, n / 1000 % 1000, n % 1000)
}

fn default_text(d: &ColumnDefault) -> String {
    match d {
        ColumnDefault::Literal(Value::Null) => "NULL".into(),
        ColumnDefault::Literal(Value::Integer(i)) => i.to_string(),
        ColumnDefault::Literal(Value::Real(f)) => format!("{f:?}"),
        ColumnDefault::Literal(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        ColumnDefault::Literal(Value::Blob(b)) => format!("X'{}'", super::dump::hex(b)),
        ColumnDefault::Expression(e) => e.clone(),
    }
}

fn table_json(t: &Table) -> Json {
    let columns = t
        .columns
        .iter()
        .map(|c| {
            Json::obj([
                ("name", Json::str(&c.name)),
                ("type", Json::str(&c.declared_type)),
                ("primary_key", Json::Bool(c.primary_key)),
                ("not_null", Json::Bool(c.not_null)),
                (
                    "default",
                    Json::opt_str(c.default.as_ref().map(default_text)),
                ),
                (
                    "generated",
                    Json::opt_str(c.generated.map(|g| {
                        if g == Generated::Stored {
                            "stored"
                        } else {
                            "virtual"
                        }
                    })),
                ),
            ])
        })
        .collect();
    Json::obj([
        ("name", Json::str(&t.name)),
        ("root_page", Json::UInt(u64::from(t.root_page))),
        ("without_rowid", Json::Bool(t.without_rowid)),
        ("readable", Json::Bool(t.unsupported.is_none())),
        ("unsupported_reason", Json::opt_str(t.unsupported.clone())),
        ("columns", Json::Arr(columns)),
    ])
}

fn index_json(i: &litewalk::Index) -> Json {
    let columns = i
        .columns
        .iter()
        .map(|c| {
            Json::obj([
                ("name", Json::opt_str(c.name.clone())),
                ("expression", Json::opt_str(c.expression.clone())),
                ("descending", Json::Bool(c.descending)),
            ])
        })
        .collect();
    Json::obj([
        ("name", Json::str(&i.name)),
        ("table", Json::str(&i.table)),
        ("root_page", Json::UInt(u64::from(i.root_page))),
        ("unique", Json::Bool(i.unique)),
        ("partial", Json::Bool(i.partial)),
        ("automatic", Json::Bool(i.automatic)),
        ("columns", Json::Arr(columns)),
    ])
}

fn index_columns_text(i: &litewalk::Index) -> String {
    if i.automatic && i.columns.is_empty() {
        return "(automatic)".into();
    }
    let cols: Vec<String> = i
        .columns
        .iter()
        .map(|c| {
            let base = c
                .name
                .clone()
                .or_else(|| c.expression.clone().map(|e| format!("({e})")))
                .unwrap_or_default();
            if c.descending {
                format!("{base} DESC")
            } else {
                base
            }
        })
        .collect();
    cols.join(", ")
}

pub(super) fn info(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let h = db.header().clone();
    let file_len = db.file_len();
    let schema = db.schema();
    let counts = schema.as_ref().ok().map(|objs| {
        (
            objs.iter().filter(|o| o.kind == ObjectKind::Table).count(),
            objs.iter().filter(|o| o.kind == ObjectKind::Index).count(),
        )
    });
    let wal_len = wal_path(&path).metadata().map(|m| m.len()).ok();
    let vacuum = match h.auto_vacuum {
        AutoVacuum::None => "none",
        AutoVacuum::Full => "full",
        AutoVacuum::Incremental => "incremental",
    };
    emit(
        args,
        out,
        || {
            Json::obj([
                ("file_size", Json::UInt(file_len)),
                (
                    "sqlite_version",
                    Json::opt_str(
                        (h.sqlite_version != 0).then(|| version_string(h.sqlite_version)),
                    ),
                ),
                ("page_size", Json::UInt(h.page_size as u64)),
                ("page_count", Json::UInt(u64::from(h.page_count))),
                ("reserved_bytes", Json::UInt(h.reserved as u64)),
                ("encoding", Json::str(h.encoding.name())),
                ("schema_format", Json::UInt(u64::from(h.schema_format))),
                ("schema_cookie", Json::UInt(u64::from(h.schema_cookie))),
                ("freelist_pages", Json::UInt(u64::from(h.freelist_count))),
                ("freelist_trunk", Json::UInt(u64::from(h.freelist_trunk))),
                ("application_id", Json::UInt(u64::from(h.application_id))),
                ("user_version", Json::UInt(u64::from(h.user_version))),
                ("auto_vacuum", Json::str(vacuum)),
                (
                    "file_format_write_version",
                    Json::UInt(u64::from(h.write_version)),
                ),
                (
                    "file_format_read_version",
                    Json::UInt(u64::from(h.read_version)),
                ),
                ("wal_mode", Json::Bool(h.wal())),
                ("wal_file_bytes", wal_len.map_or(Json::Null, Json::UInt)),
                (
                    "tables",
                    counts.map_or(Json::Null, |c| Json::UInt(c.0 as u64)),
                ),
                (
                    "indexes",
                    counts.map_or(Json::Null, |c| Json::UInt(c.1 as u64)),
                ),
                (
                    "schema_error",
                    Json::opt_str(schema.as_ref().err().map(ToString::to_string)),
                ),
            ])
        },
        |s| {
            let mut line = |k: &str, v: String| {
                let _ = writeln!(s, "{k:<18}{v}");
            };
            line("file size:", format!("{file_len} bytes"));
            if h.sqlite_version != 0 {
                line(
                    "last written by:",
                    format!("SQLite {}", version_string(h.sqlite_version)),
                );
            }
            line("page size:", h.page_size.to_string());
            line("page count:", h.page_count.to_string());
            line("reserved bytes:", h.reserved.to_string());
            line("encoding:", h.encoding.name().into());
            line("schema format:", h.schema_format.to_string());
            line("schema cookie:", h.schema_cookie.to_string());
            line(
                "freelist pages:",
                if h.freelist_count == 0 {
                    "0".into()
                } else {
                    format!(
                        "{} (first trunk: page {})",
                        h.freelist_count, h.freelist_trunk
                    )
                },
            );
            line("application id:", format!("{:#010x}", h.application_id));
            line("user version:", h.user_version.to_string());
            line("auto-vacuum:", vacuum.into());
            line(
                "journal mode:",
                if h.wal() {
                    "WAL".into()
                } else {
                    "rollback".into()
                },
            );
            match wal_len {
                Some(n) if n > 0 && args.with_wal => line(
                    "WAL file:",
                    format!("present, {n} bytes (committed frames applied)"),
                ),
                Some(n) if n > 0 => line(
                    "WAL file:",
                    format!("present, {n} bytes (not applied; use --with-wal)"),
                ),
                Some(_) => line("WAL file:", "present, empty".into()),
                None => line("WAL file:", "absent".into()),
            }
            match (&counts, &schema) {
                (Some((t, i)), _) => {
                    line("tables:", t.to_string());
                    line("indexes:", i.to_string());
                }
                (None, Err(e)) => line("schema:", format!("unreadable: {e}")),
                _ => {}
            }
        },
    )
}

pub(super) fn tables(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let tables = db.tables()?;
    emit(
        args,
        out,
        || Json::Arr(tables.iter().map(table_json).collect()),
        |s| {
            for t in &tables {
                match &t.unsupported {
                    None => {
                        let _ = writeln!(s, "{}\t{}", t.name, t.column_names().join(", "));
                    }
                    Some(why) => {
                        let _ = writeln!(s, "{}\t(unsupported: {why})", t.name);
                    }
                }
            }
        },
    )
}

pub(super) fn indexes(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let indexes = db.indexes()?;
    emit(
        args,
        out,
        || Json::Arr(indexes.iter().map(index_json).collect()),
        |s| {
            let w = indexes
                .iter()
                .map(|i| i.name.len())
                .max()
                .unwrap_or(5)
                .max(5);
            let wt = indexes
                .iter()
                .map(|i| i.table.len())
                .max()
                .unwrap_or(5)
                .max(5);
            let _ = writeln!(s, "{:<w$}  {:<wt$}  columns", "index", "table");
            for i in &indexes {
                let mut flags = String::new();
                if i.unique {
                    flags.push_str(" [unique]");
                }
                if i.partial {
                    flags.push_str(" [partial]");
                }
                let _ = writeln!(
                    s,
                    "{:<w$}  {:<wt$}  {}{flags}",
                    i.name,
                    i.table,
                    index_columns_text(i)
                );
            }
        },
    )
}

pub(super) fn objects(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let objects = db.schema()?;
    emit(
        args,
        out,
        || {
            Json::Arr(
                objects
                    .iter()
                    .map(|o| {
                        Json::obj([
                            ("type", Json::str(o.kind.as_str())),
                            ("name", Json::str(&o.name)),
                            ("table", Json::str(&o.table_name)),
                            ("root_page", Json::UInt(u64::from(o.root_page))),
                        ])
                    })
                    .collect(),
            )
        },
        |s| {
            for o in &objects {
                let _ = writeln!(s, "{:<9} {}", o.kind.as_str().to_uppercase(), o.name);
            }
        },
    )
}

pub(super) fn schema(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    extra_args(args, 1)?;
    let mut db = open(args, &path)?;
    let objects = db.schema()?;
    let tables = db.tables()?;
    let indexes = db.indexes()?;
    let named = |kind: ObjectKind| objects.iter().filter(move |o| o.kind == kind);
    emit(
        args,
        out,
        || {
            let simple = |kind: ObjectKind| {
                Json::Arr(
                    named(kind)
                        .map(|o| {
                            Json::obj([
                                ("name", Json::str(&o.name)),
                                ("table", Json::str(&o.table_name)),
                                ("sql", Json::opt_str(o.sql.clone())),
                            ])
                        })
                        .collect(),
                )
            };
            Json::obj([
                (
                    "tables",
                    Json::Arr(
                        tables
                            .iter()
                            .map(|t| {
                                let mut j = table_json(t);
                                if let (Json::Obj(f), Some(o)) = (
                                    &mut j,
                                    objects
                                        .iter()
                                        .find(|o| o.kind == ObjectKind::Table && o.name == t.name),
                                ) {
                                    f.push(("sql".into(), Json::opt_str(o.sql.clone())));
                                }
                                j
                            })
                            .collect(),
                    ),
                ),
                (
                    "indexes",
                    Json::Arr(
                        indexes
                            .iter()
                            .map(|i| {
                                let mut j = index_json(i);
                                if let (Json::Obj(f), Some(o)) = (
                                    &mut j,
                                    objects
                                        .iter()
                                        .find(|o| o.kind == ObjectKind::Index && o.name == i.name),
                                ) {
                                    f.push(("sql".into(), Json::opt_str(o.sql.clone())));
                                }
                                j
                            })
                            .collect(),
                    ),
                ),
                ("views", simple(ObjectKind::View)),
                ("triggers", simple(ObjectKind::Trigger)),
            ])
        },
        |s| {
            for t in &tables {
                let kind = if t.without_rowid {
                    " WITHOUT ROWID"
                } else {
                    ""
                };
                let _ = writeln!(s, "TABLE {}  (root page {}{kind})", t.name, t.root_page);
                if let Some(why) = &t.unsupported {
                    let _ = writeln!(s, "  unsupported: {why}");
                }
                for c in &t.columns {
                    let mut notes = Vec::new();
                    if c.primary_key {
                        notes.push("PRIMARY KEY".to_owned());
                    }
                    if c.not_null {
                        notes.push("NOT NULL".into());
                    }
                    if let Some(d) = &c.default {
                        notes.push(format!("DEFAULT {}", default_text(d)));
                    }
                    match c.generated {
                        Some(Generated::Stored) => notes.push("GENERATED STORED".into()),
                        Some(Generated::Virtual) => notes.push("GENERATED VIRTUAL".into()),
                        None => {}
                    }
                    let ty = if c.declared_type.is_empty() {
                        "-"
                    } else {
                        &c.declared_type
                    };
                    let _ = writeln!(s, "  {:<20} {:<14} {}", c.name, ty, notes.join(", "));
                }
                if args.verbose {
                    if let Some(sql) = objects
                        .iter()
                        .find(|o| o.kind == ObjectKind::Table && o.name == t.name)
                        .and_then(|o| o.sql.as_ref())
                    {
                        let _ = writeln!(s, "  sql: {sql}");
                    }
                }
            }
            for i in &indexes {
                let _ = writeln!(
                    s,
                    "INDEX {} ON {} ({})  (root page {})",
                    i.name,
                    i.table,
                    index_columns_text(i),
                    i.root_page
                );
            }
            for o in named(ObjectKind::View) {
                let _ = writeln!(s, "VIEW {}", o.name);
                if args.verbose {
                    let _ = writeln!(s, "  sql: {}", o.sql.as_deref().unwrap_or(""));
                }
            }
            for o in named(ObjectKind::Trigger) {
                let _ = writeln!(s, "TRIGGER {}  (on {})", o.name, o.table_name);
                if args.verbose {
                    let _ = writeln!(s, "  sql: {}", o.sql.as_deref().unwrap_or(""));
                }
            }
        },
    )
}
