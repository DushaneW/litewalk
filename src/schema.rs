//! Minimal reader for the SQL text stored in `sqlite_schema`: column definitions, rowid
//! aliases, defaults, generated columns, WITHOUT ROWID primary keys and index columns.
//! It tokenizes; it does not parse expressions and never evaluates them.

use crate::record::Value;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Ident(String),
    Str(String),
    Num(String),
    Punct(char),
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            '"' | '`' | '[' | '\'' => {
                let close = if c == '[' { ']' } else { c };
                i += 1;
                let mut s = String::new();
                while i < chars.len() {
                    if chars[i] == close {
                        if close != ']' && chars.get(i + 1) == Some(&close) {
                            s.push(close);
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    s.push(chars[i]);
                    i += 1;
                }
                i += 1;
                out.push(if c == '\'' {
                    Tok::Str(s)
                } else {
                    Tok::Ident(s)
                });
            }
            c if c.is_ascii_digit()
                || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let start = i;
                while i < chars.len() {
                    let d = chars[i];
                    let exp_sign = (d == '+' || d == '-')
                        && matches!(chars[i - 1], 'e' | 'E')
                        && !chars[start..i].iter().any(|&x| x == 'x' || x == 'X');
                    if d.is_ascii_alphanumeric() || d == '.' || d == '_' || exp_sign {
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(Tok::Num(chars[start..i].iter().collect()));
            }
            c if c.is_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                out.push(Tok::Word(chars[start..i].iter().collect()));
            }
            _ => {
                out.push(Tok::Punct(c));
                i += 1;
            }
        }
    }
    out
}

fn is_word(t: &Tok, w: &str) -> bool {
    matches!(t, Tok::Word(x) if x.eq_ignore_ascii_case(w))
}

fn name_of(t: &Tok) -> Option<&str> {
    match t {
        Tok::Word(n) | Tok::Ident(n) => Some(n),
        _ => None,
    }
}

/// Renders tokens back to approximate SQL, for display of expressions.
fn render(toks: &[Tok]) -> String {
    let mut out = String::new();
    for t in toks {
        let piece = match t {
            Tok::Word(w) | Tok::Num(w) => w.clone(),
            Tok::Ident(i) => format!("\"{}\"", i.replace('"', "\"\"")),
            Tok::Str(s) => format!("'{}'", s.replace('\'', "''")),
            Tok::Punct(c) => c.to_string(),
        };
        let glue = matches!(t, Tok::Punct('(' | ')' | ',' | '.')) || out.ends_with(['(', '.']);
        if !out.is_empty() && !glue {
            out.push(' ');
        }
        out.push_str(&piece);
    }
    out
}

/// Splits `toks[start..]` at top-level commas up to the matching close paren.
/// Returns the segments and the index of the closing paren (or `toks.len()`).
fn split_group(toks: &[Tok], start: usize) -> (Vec<Vec<Tok>>, usize) {
    let mut depth = 0usize;
    let mut segments: Vec<Vec<Tok>> = vec![Vec::new()];
    for (i, t) in toks.iter().enumerate().skip(start) {
        match t {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') if depth == 0 => return (segments, i),
            Tok::Punct(')') => depth -= 1,
            Tok::Punct(',') if depth == 0 => {
                segments.push(Vec::new());
                continue;
            }
            _ => {}
        }
        segments.last_mut().expect("never empty").push(t.clone());
    }
    (segments, toks.len())
}

/// Positions of tokens at parenthesis depth 0.
fn top_level(seg: &[Tok]) -> Vec<usize> {
    let mut depth = 0usize;
    let mut out = Vec::new();
    for (i, t) in seg.iter().enumerate() {
        match t {
            Tok::Punct('(') => {
                if depth == 0 {
                    out.push(i);
                }
                depth += 1;
            }
            Tok::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(i),
            _ => {}
        }
    }
    out
}

/// Column affinity (<https://www.sqlite.org/datatype3.html#determination_of_column_affinity>).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Declared type contains "INT".
    Integer,
    /// Declared type contains "CHAR", "CLOB" or "TEXT".
    Text,
    /// Declared type is empty or contains "BLOB".
    Blob,
    /// Declared type contains "REAL", "FLOA" or "DOUB".
    Real,
    /// Everything else.
    Numeric,
}

/// Applies the affinity rules in their documented order.
pub fn affinity_of(declared_type: &str) -> Affinity {
    let t = declared_type.to_ascii_uppercase();
    let has = |n: &str| t.contains(n);
    if has("INT") {
        Affinity::Integer
    } else if has("CHAR") || has("CLOB") || has("TEXT") {
        Affinity::Text
    } else if t.is_empty() || has("BLOB") {
        Affinity::Blob
    } else if has("REAL") || has("FLOA") || has("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// Kind of a generated column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generated {
    /// Computed on read and not stored in the file. litewalk has no SQL engine and cannot read it.
    Virtual,
    /// Computed on write and stored in the row like any other column.
    Stored,
}

/// A column's `DEFAULT`.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnDefault {
    /// A constant: NULL, number, string, blob, TRUE or FALSE.
    Literal(Value),
    /// Anything else, kept as (approximate) SQL text. litewalk does not evaluate it.
    Expression(String),
}

/// A column as declared in `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Declared type, as written (may be empty).
    pub declared_type: String,
    /// True for a column-level `PRIMARY KEY` or a member of a table-level one.
    pub primary_key: bool,
    /// True if declared `NOT NULL`.
    pub not_null: bool,
    /// `DEFAULT`, if declared.
    pub default: Option<ColumnDefault>,
    /// Set for generated columns.
    pub generated: Option<Generated>,
}

impl Column {
    /// The column's type affinity.
    pub fn affinity(&self) -> Affinity {
        affinity_of(&self.declared_type)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableDef {
    pub columns: Vec<Column>,
    pub rowid_alias: Option<usize>,
    pub without_rowid: bool,
    /// Indexes of the PRIMARY KEY columns in key order (WITHOUT ROWID tables).
    pub pk_columns: Vec<usize>,
    /// Set when the table cannot be read by this crate, with the reason.
    pub unsupported: Option<String>,
}

const CONSTRAINT_STARTS: [&str; 5] = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];
const COLUMN_CONSTRAINT_WORDS: [&str; 11] = [
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
];

fn literal(toks: &[Tok]) -> Option<Value> {
    match toks {
        [Tok::Word(w)] if w.eq_ignore_ascii_case("NULL") => Some(Value::Null),
        [Tok::Word(w)] if w.eq_ignore_ascii_case("TRUE") => Some(Value::Integer(1)),
        [Tok::Word(w)] if w.eq_ignore_ascii_case("FALSE") => Some(Value::Integer(0)),
        [Tok::Str(s)] => Some(Value::Text(s.clone())),
        [Tok::Word(x), Tok::Str(h)] if x.eq_ignore_ascii_case("X") && h.len() % 2 == 0 => {
            let bytes: Option<Vec<u8>> = (0..h.len())
                .step_by(2)
                .map(|i| h.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
                .collect();
            bytes.map(Value::Blob)
        }
        [Tok::Num(n)] => number(n, false),
        [Tok::Punct(sign @ ('-' | '+')), Tok::Num(n)] => number(n, *sign == '-'),
        _ => None,
    }
}

fn number(text: &str, negative: bool) -> Option<Value> {
    let hex = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"));
    if let Some(h) = hex {
        let v = u64::from_str_radix(h, 16).ok()? as i64;
        return Some(Value::Integer(if negative { v.wrapping_neg() } else { v }));
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let signed = if negative {
            format!("-{text}")
        } else {
            text.to_owned()
        };
        if let Ok(i) = signed.parse::<i64>() {
            return Some(Value::Integer(i));
        }
    }
    let f: f64 = text.parse().ok()?;
    Some(Value::Real(if negative { -f } else { f }))
}

fn parse_default(seg: &[Tok], after: usize) -> ColumnDefault {
    let rest = &seg[after..];
    let take = match rest {
        [Tok::Punct('('), ..] => {
            let (_, close) = split_group(rest, 1);
            (close + 1).min(rest.len())
        }
        [Tok::Punct('-' | '+'), Tok::Num(_), ..] => 2,
        [Tok::Word(x), Tok::Str(_), ..] if x.eq_ignore_ascii_case("X") => 2,
        [] => 0,
        _ => 1,
    };
    let toks = &rest[..take];
    match literal(toks) {
        Some(v) => ColumnDefault::Literal(v),
        None => ColumnDefault::Expression(render(toks)),
    }
}

pub(crate) fn parse_create_table(sql: &str) -> Option<TableDef> {
    let toks = tokenize(sql);
    let open = toks.iter().position(|t| *t == Tok::Punct('('))?;
    let (segments, end) = split_group(&toks, open + 1);

    let mut columns: Vec<Column> = Vec::new();
    let mut alias: Option<usize> = None;
    let mut table_pk: Vec<String> = Vec::new();
    let mut unsupported = None;

    for seg in &segments {
        let Some(first) = seg.first() else { continue };
        if CONSTRAINT_STARTS.iter().any(|w| is_word(first, w)) {
            if let Some(p) = seg.iter().position(|t| is_word(t, "PRIMARY")) {
                if let [k, Tok::Punct('('), ..] = &seg[p + 1..] {
                    if is_word(k, "KEY") {
                        let (parts, _) = split_group(&seg[p + 1..], 2);
                        table_pk = parts
                            .iter()
                            .filter_map(|part| part.first().and_then(name_of).map(str::to_owned))
                            .collect();
                    }
                }
            }
            continue;
        }
        let Some(name) = name_of(first).map(str::to_owned) else {
            continue;
        };
        let top = top_level(seg);
        let at = |i: usize, w: &str| top.contains(&i) && is_word(&seg[i], w);

        let mut ty = Vec::new();
        for t in &seg[1..] {
            match t {
                Tok::Word(w)
                    if COLUMN_CONSTRAINT_WORDS
                        .iter()
                        .any(|c| w.eq_ignore_ascii_case(c)) =>
                {
                    break
                }
                Tok::Word(w) => ty.push(w.clone()),
                _ => break,
            }
        }

        let mut col = Column {
            name,
            declared_type: ty.join(" "),
            primary_key: false,
            not_null: false,
            default: None,
            generated: None,
        };
        let mut pk_desc = false;
        for i in 0..seg.len() {
            if at(i, "PRIMARY") && seg.get(i + 1).is_some_and(|t| is_word(t, "KEY")) {
                col.primary_key = true;
                pk_desc = seg.get(i + 2).is_some_and(|t| is_word(t, "DESC"));
            } else if at(i, "NOT") && seg.get(i + 1).is_some_and(|t| is_word(t, "NULL")) {
                col.not_null = true;
            } else if at(i, "DEFAULT") && col.default.is_none() {
                col.default = Some(parse_default(seg, i + 1));
            } else if at(i, "AS") && seg.get(i + 1) == Some(&Tok::Punct('(')) {
                let (_, close) = split_group(seg, i + 2);
                let stored = seg.get(close + 1).is_some_and(|t| is_word(t, "STORED"));
                col.generated = Some(if stored {
                    Generated::Stored
                } else {
                    Generated::Virtual
                });
            }
        }
        if col.generated == Some(Generated::Virtual) {
            unsupported = Some(format!(
                "VIRTUAL generated column \"{}\" cannot be evaluated without an SQL engine",
                col.name
            ));
        }
        if col.primary_key
            && alias.is_none()
            && col.declared_type.eq_ignore_ascii_case("INTEGER")
            && !pk_desc
        {
            alias = Some(columns.len());
        }
        columns.push(col);
    }

    let tail = &toks[(end + 1).min(toks.len())..];
    let without_rowid = tail
        .windows(2)
        .any(|w| is_word(&w[0], "WITHOUT") && is_word(&w[1], "ROWID"));

    let mut pk_columns: Vec<usize> = Vec::new();
    if !table_pk.is_empty() {
        for name in &table_pk {
            if let Some(i) = columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
            {
                columns[i].primary_key = true;
                pk_columns.push(i);
            }
        }
        if alias.is_none()
            && pk_columns.len() == 1
            && columns[pk_columns[0]]
                .declared_type
                .eq_ignore_ascii_case("INTEGER")
        {
            alias = Some(pk_columns[0]);
        }
    } else if let Some(i) = columns.iter().position(|c| c.primary_key) {
        pk_columns.push(i);
    }
    if without_rowid {
        alias = None;
        if pk_columns.is_empty() {
            unsupported = Some("WITHOUT ROWID table without a readable PRIMARY KEY".into());
        }
    }
    Some(TableDef {
        columns,
        rowid_alias: alias,
        without_rowid,
        pk_columns,
        unsupported,
    })
}

/// One element of an index's column list.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexColumn {
    /// Column name, or `None` when the element is an expression.
    pub name: Option<String>,
    /// Expression text (approximate) when `name` is `None`.
    pub expression: Option<String>,
    /// True for `DESC`.
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexDef {
    pub unique: bool,
    pub table: String,
    pub columns: Vec<IndexColumn>,
    pub partial: bool,
}

pub(crate) fn parse_create_index(sql: &str) -> Option<IndexDef> {
    let toks = tokenize(sql);
    let unique = toks.iter().take(3).any(|t| is_word(t, "UNIQUE"));
    let on = toks.iter().position(|t| is_word(t, "ON"))?;
    let mut table_at = on + 1;
    if toks.get(table_at + 1) == Some(&Tok::Punct('.')) {
        table_at += 2;
    }
    let table = name_of(toks.get(table_at)?)?.to_owned();
    if toks.get(table_at + 1) != Some(&Tok::Punct('(')) {
        return None;
    }
    let (segments, end) = split_group(&toks, table_at + 2);
    let columns = segments
        .iter()
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let descending = seg.iter().any(|t| is_word(t, "DESC"));
            let plain = seg.iter().skip(1).all(|t| {
                is_word(t, "ASC")
                    || is_word(t, "DESC")
                    || is_word(t, "COLLATE")
                    || matches!(t, Tok::Word(_) | Tok::Ident(_))
            });
            match (name_of(&seg[0]), plain) {
                (Some(n), true) => IndexColumn {
                    name: Some(n.to_owned()),
                    expression: None,
                    descending,
                },
                _ => IndexColumn {
                    name: None,
                    expression: Some(render(seg)),
                    descending,
                },
            }
        })
        .collect();
    let partial = toks
        .get(end + 1..)
        .is_some_and(|t| t.iter().any(|x| is_word(x, "WHERE")));
    Some(IndexDef {
        unique,
        table,
        columns,
        partial,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(sql: &str) -> TableDef {
        parse_create_table(sql).expect("parses")
    }

    fn names(d: &TableDef) -> Vec<&str> {
        d.columns.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn plain_columns_and_types() {
        let d = def("CREATE TABLE t (a, b INT, c VARCHAR(10), d DOUBLE PRECISION)");
        assert_eq!(names(&d), ["a", "b", "c", "d"]);
        assert_eq!(d.columns[3].declared_type, "DOUBLE PRECISION");
        assert_eq!(d.rowid_alias, None);
    }

    #[test]
    fn affinity_follows_the_documented_rule_order() {
        let a = |t: &str| affinity_of(t);
        assert_eq!(a("REAL"), Affinity::Real);
        assert_eq!(a("DOUBLE PRECISION"), Affinity::Real);
        assert_eq!(a("FLOATING POINT"), Affinity::Integer); // contains "INT": rule 1 wins
        assert_eq!(a("VARCHAR(10)"), Affinity::Text);
        assert_eq!(a(""), Affinity::Blob);
        assert_eq!(a("NUMERIC"), Affinity::Numeric);
        assert_eq!(a("BOOLEAN"), Affinity::Numeric);
    }

    #[test]
    fn integer_primary_key_is_a_rowid_alias_only_in_its_exact_form() {
        assert_eq!(
            def("CREATE TABLE t (id INTEGER PRIMARY KEY, x)").rowid_alias,
            Some(0)
        );
        assert_eq!(
            def("CREATE TABLE t (x, id integer primary key autoincrement)").rowid_alias,
            Some(1)
        );
        assert_eq!(
            def("CREATE TABLE t (id INT PRIMARY KEY, x)").rowid_alias,
            None
        );
        assert_eq!(
            def("CREATE TABLE t (id INTEGER PRIMARY KEY DESC, x)").rowid_alias,
            None
        );
        assert_eq!(
            def("CREATE TABLE t (x, id INTEGER, PRIMARY KEY (id))").rowid_alias,
            Some(1)
        );
        assert_eq!(
            def("CREATE TABLE t (a INTEGER, b INTEGER, PRIMARY KEY (a, b))").rowid_alias,
            None
        );
    }

    #[test]
    fn quoted_identifiers_comments_and_nested_parens() {
        let d = def("CREATE TABLE \"my t\" (\"a,b\" TEXT, [c d] NUMERIC(10, 2) DEFAULT (1+2), `e` /* x, y */ BLOB)");
        assert_eq!(names(&d), ["a,b", "c d", "e"]);
    }

    #[test]
    fn reads_defaults_as_literals_or_expressions() {
        let d = def(
            "CREATE TABLE t (a DEFAULT 5, b DEFAULT -2.5, c DEFAULT 'it''s', d DEFAULT NULL, e DEFAULT X'ff00', \
             f DEFAULT TRUE, g DEFAULT CURRENT_TIMESTAMP, h DEFAULT (1 + 2), i NOT NULL DEFAULT 0x10, j)",
        );
        let lit = |i: usize| match &d.columns[i].default {
            Some(ColumnDefault::Literal(v)) => v.clone(),
            other => panic!("column {i}: {other:?}"),
        };
        assert_eq!(lit(0), Value::Integer(5));
        assert_eq!(lit(1), Value::Real(-2.5));
        assert_eq!(lit(2), Value::Text("it's".into()));
        assert_eq!(lit(3), Value::Null);
        assert_eq!(lit(4), Value::Blob(vec![0xff, 0]));
        assert_eq!(lit(5), Value::Integer(1));
        assert_eq!(
            d.columns[6].default,
            Some(ColumnDefault::Expression("CURRENT_TIMESTAMP".into()))
        );
        assert_eq!(
            d.columns[7].default,
            Some(ColumnDefault::Expression("(1 + 2)".into()))
        );
        assert_eq!(lit(8), Value::Integer(16));
        assert!(d.columns[8].not_null);
        assert_eq!(d.columns[9].default, None);
    }

    #[test]
    fn generated_columns_are_classified() {
        let d = def("CREATE TABLE t (a, b AS (a + 1) STORED, c GENERATED ALWAYS AS (CAST(a AS TEXT)) VIRTUAL, d AS (a))");
        assert_eq!(d.columns[1].generated, Some(Generated::Stored));
        assert_eq!(d.columns[2].generated, Some(Generated::Virtual));
        assert_eq!(d.columns[3].generated, Some(Generated::Virtual));
        assert!(d.unsupported.is_some());
        let only_stored = def("CREATE TABLE t (a, b AS (a + 1) STORED)");
        assert!(only_stored.unsupported.is_none());
        // CAST(.. AS ..) inside DEFAULT or CHECK is not a generated column
        let d = def("CREATE TABLE t (a DEFAULT (CAST(1 AS TEXT)) CHECK (a IS NOT NULL))");
        assert_eq!(d.columns[0].generated, None);
    }

    #[test]
    fn without_rowid_primary_keys() {
        let d = def("CREATE TABLE t (a, b, c, PRIMARY KEY (c DESC, a)) WITHOUT ROWID");
        assert!(d.without_rowid);
        assert_eq!(d.pk_columns, [2, 0]);
        assert!(d.unsupported.is_none());
        let d = def("CREATE TABLE t (k TEXT PRIMARY KEY, v) WITHOUT ROWID");
        assert_eq!(d.pk_columns, [0]);
        assert_eq!(d.rowid_alias, None);
    }

    #[test]
    fn index_columns() {
        let d = parse_create_index("CREATE UNIQUE INDEX \"ix\" ON main.t (a, b DESC, lower(c), d COLLATE nocase) WHERE a > 1").unwrap();
        assert!(d.unique && d.partial);
        assert_eq!(d.table, "t");
        let cols: Vec<_> = d
            .columns
            .iter()
            .map(|c| (c.name.as_deref(), c.descending))
            .collect();
        assert_eq!(
            cols,
            [
                (Some("a"), false),
                (Some("b"), true),
                (None, false),
                (Some("d"), false)
            ]
        );
        assert_eq!(d.columns[2].expression.as_deref(), Some("lower(c)"));
    }
}
