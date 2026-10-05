//! Minimal JSON writer. Output structures are built by hand so the format is intentional and
//! stable rather than a by-product of internal types.

use std::fmt::Write;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn opt_str(s: Option<impl Into<String>>) -> Json {
        s.map_or(Json::Null, |s| Json::Str(s.into()))
    }

    pub fn obj<const N: usize>(fields: [(&str, Json); N]) -> Json {
        Json::Obj(fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    pub fn render(&self, pretty: bool) -> String {
        let mut out = String::new();
        self.write(&mut out, pretty, 0);
        out
    }

    fn write(&self, out: &mut String, pretty: bool, depth: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Json::UInt(u) => {
                let _ = write!(out, "{u}");
            }
            Json::Float(f) if f.is_finite() => {
                let _ = write!(out, "{f:?}");
            }
            // JSON has no infinities; callers that care convert them to strings first.
            Json::Float(_) => out.push_str("null"),
            Json::Str(s) => escape(out, s),
            Json::Arr(items) => {
                if items.is_empty() {
                    return out.push_str("[]");
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    separator(out, pretty, depth + 1, i == 0);
                    item.write(out, pretty, depth + 1);
                }
                close(out, pretty, depth, ']');
            }
            Json::Obj(fields) => {
                if fields.is_empty() {
                    return out.push_str("{}");
                }
                out.push('{');
                for (i, (k, v)) in fields.iter().enumerate() {
                    separator(out, pretty, depth + 1, i == 0);
                    escape(out, k);
                    out.push_str(if pretty { ": " } else { ":" });
                    v.write(out, pretty, depth + 1);
                }
                close(out, pretty, depth, '}');
            }
        }
    }
}

fn separator(out: &mut String, pretty: bool, depth: usize, first: bool) {
    if !first {
        out.push(',');
    }
    if pretty {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    }
}

fn close(out: &mut String, pretty: bool, depth: usize, bracket: char) {
    if pretty {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    }
    out.push(bracket);
}

fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_compact_and_pretty() {
        let v = Json::obj([
            ("a", Json::Int(-1)),
            (
                "b",
                Json::Arr(vec![Json::Null, Json::Bool(true), Json::Float(1.0)]),
            ),
            ("c", Json::Arr(vec![])),
        ]);
        assert_eq!(v.render(false), r#"{"a":-1,"b":[null,true,1.0],"c":[]}"#);
        assert_eq!(
            v.render(true),
            "{\n  \"a\": -1,\n  \"b\": [\n    null,\n    true,\n    1.0\n  ],\n  \"c\": []\n}"
        );
    }

    #[test]
    fn escapes_control_characters_quotes_and_unicode() {
        let s = Json::str("a\"b\\c\n\t\u{1}é漢😀");
        assert_eq!(s.render(false), "\"a\\\"b\\\\c\\n\\t\\u0001é漢😀\"");
    }

    #[test]
    fn non_finite_floats_become_null() {
        assert_eq!(Json::Float(f64::INFINITY).render(false), "null");
        assert_eq!(Json::Float(1e300).render(false), "1e300");
        assert_eq!(Json::UInt(u64::MAX).render(false), "18446744073709551615");
    }
}
