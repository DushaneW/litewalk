//! Command-line parsing. Small and explicit; no dependencies.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Tsv,
    Csv,
    Json,
}

#[derive(Debug, Default, Clone)]
pub struct Args {
    pub command: String,
    pub positionals: Vec<String>,
    pub json: bool,
    pub verbose: bool,
    pub limit: Option<usize>,
    pub offset: usize,
    pub format: Option<Format>,
    pub columns: Option<Vec<String>>,
    pub rowid: bool,
    pub pretty: bool,
    pub hex_blobs: bool,
    pub hex: bool,
    pub cache: Option<usize>,
    pub with_wal: bool,
    pub free_space: bool,
}

fn number(flag: &str, value: Option<String>) -> Result<usize, String> {
    let v = value.ok_or_else(|| format!("{flag} needs a value"))?;
    v.parse()
        .map_err(|_| format!("{flag} needs a non-negative integer, got \"{v}\""))
}

/// Parses everything after the program name. `Ok(None)` means `--help` or `--version` was handled.
pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = argv.iter().cloned();
    let mut positionals = Vec::new();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let value = |it: &mut dyn Iterator<Item = String>| inline.clone().or_else(|| it.next());
        match flag.as_str() {
            "--json" => a.json = true,
            "--verbose" | "-v" => a.verbose = true,
            "--rowid" => a.rowid = true,
            "--pretty" => a.pretty = true,
            "--hex-blobs" => a.hex_blobs = true,
            "--hex" => a.hex = true,
            "--with-wal" => a.with_wal = true,
            "--free-space" => a.free_space = true,
            "--limit" => a.limit = Some(number(&flag, value(&mut it))?),
            "--offset" => a.offset = number(&flag, value(&mut it))?,
            "--cache" => a.cache = Some(number(&flag, value(&mut it))?),
            "--columns" => {
                let v = value(&mut it).ok_or("--columns needs a comma-separated list")?;
                a.columns = Some(
                    v.split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "--format" => {
                a.format = Some(match value(&mut it).as_deref() {
                    Some("text") => Format::Text,
                    Some("tsv") => Format::Tsv,
                    Some("csv") => Format::Csv,
                    Some("json") => Format::Json,
                    other => {
                        return Err(format!(
                            "--format must be one of text, tsv, csv, json (got {other:?})"
                        ))
                    }
                })
            }
            f if f.starts_with('-') && f != "-" => return Err(format!("unknown option {f}")),
            _ => positionals.push(arg),
        }
    }
    let mut positionals = positionals.into_iter();
    a.command = positionals.next().ok_or("missing command")?;
    a.positionals = positionals.collect();
    if a.json {
        a.format = Some(Format::Json);
    }
    Ok(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_flags_in_any_position_and_both_value_styles() {
        let a = p("dump db.sqlite users --limit 5 --offset=2 --columns a,b --rowid --format csv --hex-blobs").unwrap();
        assert_eq!(
            (a.command.as_str(), a.positionals.as_slice()),
            ("dump", &["db.sqlite".to_owned(), "users".to_owned()][..])
        );
        assert_eq!(
            (a.limit, a.offset, a.rowid, a.hex_blobs, a.format),
            (Some(5), 2, true, true, Some(Format::Csv))
        );
        assert_eq!(a.columns.unwrap(), ["a", "b"]);
        assert!(p("--json info db").unwrap().json);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p("").is_err());
        assert!(p("info db --nope").is_err());
        assert!(p("dump db t --limit x").is_err());
        assert!(p("dump db t --limit").is_err());
        assert!(p("dump db t --format xml").is_err());
    }
}
