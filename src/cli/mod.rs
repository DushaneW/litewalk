//! The `litewalk` command-line interface. Machine-readable output goes to stdout, diagnostics
//! to stderr. Exit status: 0 success, 1 failure (unreadable or invalid database, I/O error),
//! 2 usage error.

mod args;
mod dump;
mod inspect;
mod integrity;
mod json;

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use litewalk::{Database, Error, Options};

use args::Args;
use json::Json;

const USAGE: &str = "\
usage: litewalk <command> <database> [arguments] [options]

inspection
  info <db>                     header fields, table and index counts
  tables <db>                   tables and their columns
  indexes <db>                  indexes and their columns
  objects <db>                  every schema object (table, index, view, trigger)
  schema <db>                   tables, columns, types, root pages, indexes, views, triggers
  page <db> <n> [--hex]         decode one page
  map <db>                      which object owns each page
  wal <db>                      inspect <db>-wal

data
  dump <db> <table>             rows; --format text|tsv|csv|json --limit N --offset N
                                --columns a,b --rowid --pretty --hex-blobs
  get <db> <table> <rowid>      one row by rowid

analysis
  check <db>                    structural validation (non-zero exit if invalid)
  stats <db> [--cache N]        page cache statistics for a full read
  forensic <db> [--free-space]  structural report (experimental: free space regions)

options
  --json            JSON on stdout (all commands except dump, where --format json is equivalent)
  --pretty          indent JSON
  --verbose, -v     more detail
  --with-wal        apply the committed frames of <db>-wal when reading
  --help, --version
";

/// Errors a command can end with.
pub(crate) enum CliError {
    Usage(String),
    Lib(Error),
    Io(io::Error),
    /// The command already printed its findings; only the exit status remains.
    Failed,
}

impl From<Error> for CliError {
    fn from(e: Error) -> Self {
        CliError::Lib(e)
    }
}

impl From<io::Error> for CliError {
    fn from(e: io::Error) -> Self {
        CliError::Io(e)
    }
}

pub(crate) type Out<'a> = &'a mut dyn Write;
pub(crate) type CliResult = Result<(), CliError>;

pub(crate) fn path_arg(args: &Args, n: usize, what: &str) -> Result<PathBuf, CliError> {
    args.positionals
        .get(n)
        .map(PathBuf::from)
        .ok_or_else(|| CliError::Usage(format!("{} needs {what}", args.command)))
}

pub(crate) fn extra_args(args: &Args, max: usize) -> CliResult {
    match args.positionals.get(max) {
        Some(extra) => Err(CliError::Usage(format!("unexpected argument \"{extra}\""))),
        None => Ok(()),
    }
}

pub(crate) fn wal_path(db: &Path) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    if !p.to_string_lossy().ends_with("-wal") {
        p.push("-wal");
    }
    p.into()
}

pub(crate) fn open(args: &Args, path: &Path) -> Result<Database<File>, CliError> {
    let options = Options {
        cache_pages: args.cache.unwrap_or(litewalk::DEFAULT_CACHE_PAGES),
    };
    if args.with_wal && wal_path(path).metadata().is_ok_and(|m| m.len() > 0) {
        let wal = litewalk::Wal::open(wal_path(path))?;
        return Ok(Database::with_wal(File::open(path)?, &wal, options)?);
    }
    Ok(Database::open_with(path, options)?)
}

/// Writes `json` if `--json` was given, otherwise the text produced by `text`.
pub(crate) fn emit(
    args: &Args,
    out: Out<'_>,
    json: impl FnOnce() -> Json,
    text: impl FnOnce(&mut String),
) -> CliResult {
    let mut s = String::new();
    if args.json {
        s = json().render(args.pretty);
    } else {
        text(&mut s);
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    out.write_all(s.as_bytes())?;
    Ok(())
}

fn report(err: &mut dyn Write, e: &CliError) -> i32 {
    let _ = match e {
        CliError::Usage(m) => writeln!(err, "litewalk: {m}\nTry 'litewalk --help'."),
        CliError::Io(e) if e.kind() == io::ErrorKind::BrokenPipe => return 0,
        CliError::Io(e) => writeln!(err, "litewalk: i/o error: {e}"),
        CliError::Lib(Error::Corrupt(c)) => {
            let mut s = format!(
                "litewalk: corrupt database ({}): {}\n",
                c.kind.as_str(),
                c.reason
            );
            for (k, v) in c.context() {
                s.push_str(&format!("  {k}: {v}\n"));
            }
            write!(err, "{s}")
        }
        CliError::Lib(e) => writeln!(err, "litewalk: {e}"),
        CliError::Failed => Ok(()),
    };
    match e {
        CliError::Usage(_) => 2,
        _ => 1,
    }
}

/// Runs the CLI. Returns the process exit status.
pub fn run(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        let _ = write!(out, "{USAGE}");
        return 0;
    }
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        let _ = writeln!(out, "litewalk {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    let args = match args::parse(argv) {
        Ok(a) => a,
        Err(m) => return report(err, &CliError::Usage(m)),
    };
    let result = match args.command.as_str() {
        "info" => inspect::info(&args, out),
        "tables" => inspect::tables(&args, out),
        "indexes" => inspect::indexes(&args, out),
        "objects" => inspect::objects(&args, out),
        "schema" => inspect::schema(&args, out),
        "dump" => dump::dump(&args, out),
        "get" => dump::get(&args, out),
        "page" => integrity::page(&args, out),
        "map" => integrity::map(&args, out),
        "check" => integrity::check(&args, out),
        "stats" => integrity::stats(&args, out),
        "wal" => integrity::wal(&args, out),
        "forensic" => integrity::forensic(&args, out),
        other => Err(CliError::Usage(format!("unknown command \"{other}\""))),
    };
    match result {
        Ok(()) => 0,
        Err(e) => report(err, &e),
    }
}
