use crate::error::{corrupt, CorruptionKind, Result};
use crate::header::Encoding;
use crate::varint::read_varint;

/// A column value, owning its data.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float.
    Real(f64),
    /// Text, converted to UTF-8 if the database uses UTF-16.
    Text(String),
    /// Binary data.
    Blob(Vec<u8>),
}

/// A column value borrowing from the record it was decoded from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueRef<'a> {
    /// SQL NULL.
    Null,
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float.
    Real(f64),
    /// UTF-8 text borrowed from the payload.
    Text(&'a str),
    /// Binary data borrowed from the payload.
    Blob(&'a [u8]),
}

impl Value {
    /// Borrowed view of this value.
    pub fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Value::Null => ValueRef::Null,
            Value::Integer(i) => ValueRef::Integer(*i),
            Value::Real(f) => ValueRef::Real(*f),
            Value::Text(s) => ValueRef::Text(s),
            Value::Blob(b) => ValueRef::Blob(b),
        }
    }
}

impl ValueRef<'_> {
    /// Copies the value into an owned [`Value`].
    pub fn to_owned(&self) -> Value {
        match *self {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => Value::Integer(i),
            ValueRef::Real(f) => Value::Real(f),
            ValueRef::Text(s) => Value::Text(s.to_owned()),
            ValueRef::Blob(b) => Value::Blob(b.to_vec()),
        }
    }
}

fn int_be(b: &[u8]) -> i64 {
    let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
    for &x in b {
        v = (v << 8) | i64::from(x);
    }
    v
}

/// Serial type codes (<https://www.sqlite.org/fileformat.html#record_format>).
mod serial {
    pub const NULL: u64 = 0;
    pub const INT8: u64 = 1;
    pub const INT16: u64 = 2;
    pub const INT24: u64 = 3;
    pub const INT32: u64 = 4;
    pub const INT48: u64 = 5;
    pub const INT64: u64 = 6;
    pub const FLOAT64: u64 = 7;
    pub const ZERO: u64 = 8;
    pub const ONE: u64 = 9;
    pub const RESERVED_10: u64 = 10;
    pub const RESERVED_11: u64 = 11;
    pub const FIRST_VARIABLE: u64 = 12;
}

/// Splits a record into its raw columns: `(serial type, body bytes)`.
fn split_record(payload: &[u8]) -> Result<Vec<(u64, &[u8])>> {
    let Some((header_len, mut pos)) = read_varint(payload) else {
        return Err(corrupt(
            CorruptionKind::Varint,
            "record header size is truncated",
        ));
    };
    let header_len = match usize::try_from(header_len) {
        Ok(n) if n >= pos && n <= payload.len() => n,
        _ => {
            return Err(corrupt(
                CorruptionKind::Record,
                "record header size is outside the payload",
            )
            .with_expected(format!("{pos}..={}", payload.len()), header_len))
        }
    };

    let mut types = Vec::new();
    while pos < header_len {
        let Some((t, n)) = read_varint(&payload[pos..header_len]) else {
            return Err(
                corrupt(CorruptionKind::Varint, "record serial type is truncated").with_offset(pos),
            );
        };
        types.push(t);
        pos += n;
    }

    let mut body = header_len;
    let mut columns = Vec::with_capacity(types.len());
    for t in types {
        let len: usize = match t {
            serial::NULL | serial::ZERO | serial::ONE => 0,
            serial::INT8 => 1,
            serial::INT16 => 2,
            serial::INT24 => 3,
            serial::INT32 => 4,
            serial::INT48 => 6,
            serial::INT64 | serial::FLOAT64 => 8,
            serial::RESERVED_10 | serial::RESERVED_11 => {
                return Err(
                    corrupt(CorruptionKind::Record, format!("reserved serial type {t}"))
                        .with_offset(body),
                )
            }
            n => match usize::try_from((n - serial::FIRST_VARIABLE) / 2) {
                Ok(l) => l,
                Err(_) => {
                    return Err(corrupt(
                        CorruptionKind::Record,
                        "serial type length overflows",
                    ))
                }
            },
        };
        let Some(bytes) = body.checked_add(len).and_then(|end| payload.get(body..end)) else {
            return Err(corrupt(
                CorruptionKind::Record,
                "record body extends past the payload",
            )
            .with_offset(body)
            .with_expected(
                format!("{len} more bytes"),
                payload.len().saturating_sub(body),
            ));
        };
        body += len;
        columns.push((t, bytes));
    }
    Ok(columns)
}

fn decode_text(bytes: &[u8], encoding: Encoding) -> Result<String> {
    match encoding {
        Encoding::Utf8 => std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| corrupt(CorruptionKind::Record, "text value is not valid UTF-8")),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            if bytes.len() % 2 != 0 {
                return Err(corrupt(
                    CorruptionKind::Record,
                    "UTF-16 text has an odd number of bytes",
                ));
            }
            let units = bytes.chunks_exact(2).map(|c| match encoding {
                Encoding::Utf16Le => u16::from_le_bytes([c[0], c[1]]),
                _ => u16::from_be_bytes([c[0], c[1]]),
            });
            char::decode_utf16(units)
                .collect::<std::result::Result<String, _>>()
                .map_err(|_| corrupt(CorruptionKind::Record, "text value is not valid UTF-16"))
        }
    }
}

/// Decodes a record: a varint header size, serial type codes, then the column bodies.
pub fn decode_record(payload: &[u8], encoding: Encoding) -> Result<Vec<Value>> {
    split_record(payload)?
        .into_iter()
        .map(|(t, bytes)| {
            Ok(match t {
                serial::NULL => Value::Null,
                serial::INT8..=serial::INT64 => Value::Integer(int_be(bytes)),
                serial::FLOAT64 => Value::Real(f64::from_be_bytes(
                    bytes.try_into().expect("length checked"),
                )),
                serial::ZERO => Value::Integer(0),
                serial::ONE => Value::Integer(1),
                n if n % 2 == 0 => Value::Blob(bytes.to_vec()),
                _ => Value::Text(decode_text(bytes, encoding)?),
            })
        })
        .collect()
}

/// Decodes a record without copying column data. Only UTF-8 databases are supported because
/// UTF-16 text must be transcoded and therefore cannot be borrowed.
pub fn decode_record_ref(payload: &[u8]) -> Result<Vec<ValueRef<'_>>> {
    split_record(payload)?
        .into_iter()
        .map(|(t, bytes)| {
            Ok(match t {
                serial::NULL => ValueRef::Null,
                serial::INT8..=serial::INT64 => ValueRef::Integer(int_be(bytes)),
                serial::FLOAT64 => ValueRef::Real(f64::from_be_bytes(
                    bytes.try_into().expect("length checked"),
                )),
                serial::ZERO => ValueRef::Integer(0),
                serial::ONE => ValueRef::Integer(1),
                n if n % 2 == 0 => ValueRef::Blob(bytes),
                _ => ValueRef::Text(std::str::from_utf8(bytes).map_err(|_| {
                    corrupt(CorruptionKind::Record, "text value is not valid UTF-8")
                })?),
            })
        })
        .collect()
}

/// Checks that a payload is a well-formed record without allocating its values.
pub(crate) fn validate_record(payload: &[u8]) -> Result<usize> {
    split_record(payload).map(|c| c.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const U8: Encoding = Encoding::Utf8;

    #[test]
    fn decodes_all_serial_types() {
        // header: size=9, types: NULL, 0, 1, i8, i16, f64, "ab", x'01'
        let mut rec = vec![9, 0, 8, 9, 1, 2, 7, 17, 14];
        rec.extend([0xff]); // i8 = -1
        rec.extend([0x80, 0x00]); // i16 = -32768
        rec.extend(1.5f64.to_be_bytes());
        rec.extend(*b"ab");
        rec.extend([0x01]);
        let expected = vec![
            Value::Null,
            Value::Integer(0),
            Value::Integer(1),
            Value::Integer(-1),
            Value::Integer(-32768),
            Value::Real(1.5),
            Value::Text("ab".into()),
            Value::Blob(vec![1]),
        ];
        assert_eq!(decode_record(&rec, U8).unwrap(), expected);
        let borrowed: Vec<Value> = decode_record_ref(&rec)
            .unwrap()
            .iter()
            .map(ValueRef::to_owned)
            .collect();
        assert_eq!(borrowed, expected);
        assert_eq!(validate_record(&rec).unwrap(), 8);
    }

    #[test]
    fn sign_extends_odd_widths() {
        assert_eq!(int_be(&[0xff, 0xff, 0xfe]), -2);
        assert_eq!(int_be(&[0x7f, 0xff, 0xff]), 0x7f_ffff);
        assert_eq!(int_be(&[0x80, 0, 0, 0, 0, 0]), -(1 << 47));
    }

    #[test]
    fn decodes_utf16_in_both_byte_orders() {
        // "aé😀" as UTF-16: 3 code points, 4 code units = 8 bytes; serial type 13 + 2*8 = 29
        let le = [0x61, 0x00, 0xe9, 0x00, 0x3d, 0xd8, 0x00, 0xde];
        let be = [0x00, 0x61, 0x00, 0xe9, 0xd8, 0x3d, 0xde, 0x00];
        for (bytes, enc) in [(le, Encoding::Utf16Le), (be, Encoding::Utf16Be)] {
            let mut rec = vec![2, 29];
            rec.extend(bytes);
            assert_eq!(
                decode_record(&rec, enc).unwrap(),
                [Value::Text("aé😀".into())]
            );
        }
    }

    #[test]
    fn rejects_malformed() {
        assert!(decode_record(&[], U8).is_err());
        assert!(decode_record(&[5, 1], U8).is_err()); // header claims 5 bytes
        assert!(decode_record(&[2, 1], U8).is_err()); // body missing
        assert!(decode_record(&[2, 10], U8).is_err()); // reserved type
        assert!(decode_record(&[2, 15, 0xff], U8).is_err()); // invalid UTF-8
        assert!(decode_record(&[2, 15, 0xd8], Encoding::Utf16Le).is_err()); // odd UTF-16 length
        assert!(decode_record(&[2, 17, 0x00, 0xd8], Encoding::Utf16Le).is_err()); // lone surrogate
        assert!(decode_record_ref(&[2, 15, 0xff]).is_err());
    }
}
