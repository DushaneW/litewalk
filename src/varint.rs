/// Reads a SQLite varint (1 to 9 bytes, big-endian, 7 bits per byte; the ninth byte
/// contributes all 8 bits). Returns the value and the number of bytes consumed.
pub fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..8 {
        let b = *buf.get(i)?;
        v = (v << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    let b = *buf.get(8)?;
    Some(((v << 8) | u64::from(b), 9))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn write_varint(mut v: u64) -> Vec<u8> {
        if v > 0x00ff_ffff_ffff_ffff {
            let mut out = vec![0u8; 9];
            out[8] = (v & 0xff) as u8;
            v >>= 8;
            for i in (0..8).rev() {
                out[i] = ((v & 0x7f) as u8) | 0x80;
                v >>= 7;
            }
            return out;
        }
        let mut out = vec![(v & 0x7f) as u8];
        v >>= 7;
        while v > 0 {
            out.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        out.reverse();
        out
    }

    #[test]
    fn known_encodings() {
        assert_eq!(read_varint(&[0x00]), Some((0, 1)));
        assert_eq!(read_varint(&[0x7f]), Some((127, 1)));
        assert_eq!(read_varint(&[0x81, 0x00]), Some((128, 2)));
        assert_eq!(read_varint(&[0xff; 9]), Some((u64::MAX, 9)));
    }

    #[test]
    fn truncated_input_is_none() {
        assert_eq!(read_varint(&[]), None);
        assert_eq!(read_varint(&[0x80]), None);
        assert_eq!(read_varint(&[0xff; 8]), None);
    }

    #[test]
    fn round_trips_boundaries() {
        for v in [
            0u64,
            1,
            127,
            128,
            16383,
            16384,
            1 << 21,
            1 << 28,
            1 << 35,
            1 << 42,
            1 << 49,
            (1 << 56) - 1,
            1 << 56,
            1 << 63,
            u64::MAX,
        ] {
            let enc = write_varint(v);
            assert_eq!(read_varint(&enc), Some((v, enc.len())), "value {v}");
        }
    }
}
