//! dump and get: row export as text, TSV, CSV or JSON.

use std::fmt::Write as _;

use litewalk::{Table, Value};

use super::args::Format;
use super::json::Json;
use super::{extra_args, open, path_arg, Args, CliError, CliResult, Out};

pub(super) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub(super) fn value_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Integer(i) => Json::Int(*i),
        Value::Real(f) if f.is_finite() => Json::Float(*f),
        Value::Real(f) => Json::str(if f.is_nan() {
            "NaN"
        } else if *f > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }),
        Value::Text(s) => Json::str(s),
        // Blobs are always lossless in JSON; --hex-blobs only affects the text formats.
        Value::Blob(b) => Json::obj([("blob", Json::str(hex(b)))]),
    }
}

fn text_cell(v: &Value, args: &Args, format: Format) -> String {
    match v {
        Value::Null => {
            if format == Format::Csv {
                String::new()
            } else {
                "NULL".into()
            }
        }
        Value::Integer(i) => i.to_string(),
        Value::Real(f) => format!("{f:?}"),
        Value::Text(s) => match format {
            Format::Csv => {
                if s.is_empty() || s.contains([',', '"', '\n', '\r']) {
                    format!("\"{}\"", s.replace('"', "\"\""))
                } else {
                    s.clone()
                }
            }
            _ => s
                .replace('\\', "\\\\")
                .replace('\t', "\\t")
                .replace('\n', "\\n")
                .replace('\r', "\\r"),
        },
        Value::Blob(b) if args.hex_blobs => format!("x'{}'", hex(b)),
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}

fn csv_header(name: &str) -> String {
    if name.contains([',', '"', '\n', '\r']) || name.is_empty() {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_owned()
    }
}

/// Resolves `--columns` (or all columns) to indices into `table.columns`.
fn select_columns(args: &Args, table: &Table) -> Result<Vec<usize>, CliError> {
    let Some(wanted) = &args.columns else {
        return Ok((0..table.columns.len()).collect());
    };
    wanted
        .iter()
        .map(|w| {
            table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(w))
                .ok_or_else(|| {
                    CliError::Usage(format!(
                        "table \"{}\" has no column \"{w}\" (columns: {})",
                        table.name,
                        table.column_names().join(", ")
                    ))
                })
        })
        .collect()
}

pub(super) fn dump(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    let name = args
        .positionals
        .get(1)
        .ok_or_else(|| CliError::Usage("dump needs a table name".into()))?;
    extra_args(args, 2)?;
    let format = args.format.unwrap_or(Format::Tsv);
    let mut db = open(args, &path)?;
    let table = db.table(name)?;
    let cols = select_columns(args, &table)?;
    let rows = db
        .scan(&table)?
        .skip(args.offset)
        .take(args.limit.unwrap_or(usize::MAX));

    let mut header: Vec<String> = Vec::new();
    if args.rowid {
        header.push("rowid".into());
    }
    header.extend(cols.iter().map(|&i| table.columns[i].name.clone()));

    match format {
        Format::Json => out.write_all(b"[")?,
        Format::Csv => writeln!(
            out,
            "{}",
            header
                .iter()
                .map(|h| csv_header(h))
                .collect::<Vec<_>>()
                .join(",")
        )?,
        Format::Tsv | Format::Text => writeln!(out, "{}", header.join("\t"))?,
    }

    let mut failure = None;
    let mut first = true;
    for row in rows {
        let row = match row {
            Ok(r) => r,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        let cells = cols.iter().map(|&i| &row.values[i]);
        if format == Format::Json {
            let mut fields = Vec::new();
            if args.rowid {
                fields.push(("rowid".to_owned(), row.rowid.map_or(Json::Null, Json::Int)));
            }
            fields.extend(
                cols.iter()
                    .map(|&i| (table.columns[i].name.clone(), value_json(&row.values[i]))),
            );
            let rendered = Json::Obj(fields).render(args.pretty);
            let sep = if first { "" } else { "," };
            if args.pretty {
                write!(out, "{sep}\n  {}", rendered.replace('\n', "\n  "))?;
            } else {
                write!(out, "{sep}{rendered}")?;
            }
        } else {
            let sep = if format == Format::Csv { "," } else { "\t" };
            let mut fields: Vec<String> = Vec::new();
            if args.rowid {
                fields.push(row.rowid.map_or_else(|| "NULL".into(), |r| r.to_string()));
            }
            fields.extend(cells.map(|v| text_cell(v, args, format)));
            writeln!(out, "{}", fields.join(sep))?;
        }
        first = false;
    }
    if format == Format::Json {
        // Close the array even after an error so that the partial output stays valid JSON.
        out.write_all(if args.pretty && !first {
            b"\n]\n"
        } else {
            b"]\n"
        })?;
    }
    match failure {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

pub(super) fn get(args: &Args, out: Out<'_>) -> CliResult {
    let path = path_arg(args, 0, "a database path")?;
    let name = args
        .positionals
        .get(1)
        .ok_or_else(|| CliError::Usage("get needs a table name".into()))?;
    let rowid: i64 = args
        .positionals
        .get(2)
        .ok_or_else(|| CliError::Usage("get needs a rowid".into()))?
        .parse()
        .map_err(|_| CliError::Usage("rowid must be an integer".into()))?;
    extra_args(args, 3)?;
    let mut db = open(args, &path)?;
    let table = db.table(name)?;
    let Some(row) = db.get(&table, rowid)? else {
        eprintln!("litewalk: no row with rowid {rowid} in \"{}\"", table.name);
        return Err(CliError::Failed);
    };
    if args.json {
        let mut fields = vec![("rowid".to_owned(), Json::Int(rowid))];
        fields.extend(
            table
                .columns
                .iter()
                .zip(&row.values)
                .map(|(c, v)| (c.name.clone(), value_json(v))),
        );
        writeln!(out, "{}", Json::Obj(fields).render(args.pretty))?;
    } else {
        for (c, v) in table.columns.iter().zip(&row.values) {
            writeln!(out, "{}\t{}", c.name, text_cell(v, args, Format::Tsv))?;
        }
    }
    Ok(())
}
