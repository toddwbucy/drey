//! Binary codec for the on-disk format (PRD §10.1.1).
//!
//! Binary, not text, for fidelity: `f32` embeddings and `Bytes` values must
//! round-trip byte-exact (PRD §10.2), which text float formatting cannot
//! promise. This is a hand-rolled, little-endian, schema-bound encoding —
//! smaller and faster than a self-describing codec, with forward compatibility
//! handled explicitly through the format version gate (open question 8 resolved
//! toward the tight schema-bound option for v0.1).
//!
//! Everything little-endian. Lengths are `u32`. The encoding stores every node
//! and edge id explicitly (PRD §7.4) — never by position.

use crate::error::{Error, Result};
use crate::store::{EdgeRecord, NodeRecord};
use crate::types::{Properties, Scalar, Value};

/// A growable byte sink.
#[derive(Default)]
pub(crate) struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f32(&mut self, v: f32) {
        // Byte-exact: preserve the exact bit pattern, including denormals and
        // negative zero (PRD §10.2, spec §3.2 hostile values).
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.buf.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

/// A cursor over a byte slice. Every read is bounds-checked and returns a
/// `Codec` error on a short/torn buffer, so a truncated file fails explicitly
/// rather than panicking (PRD §10.2.1 corrupt-tail row).
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return Err(Error::Codec(format!(
                "unexpected end of buffer: wanted {n} bytes at offset {}, {} remain",
                self.pos,
                self.remaining()
            )));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    pub fn str(&mut self) -> Result<String> {
        let b = self.bytes()?;
        String::from_utf8(b).map_err(|e| Error::Codec(format!("invalid utf-8: {e}")))
    }
}

// ---- Value / Scalar ----

pub(crate) fn write_scalar(w: &mut Writer, s: &Scalar) {
    match s {
        Scalar::Bool(b) => {
            w.u8(0);
            w.u8(*b as u8);
        }
        Scalar::I64(i) => {
            w.u8(1);
            w.i64(*i);
        }
        Scalar::F64(f) => {
            w.u8(2);
            w.f64(*f);
        }
        Scalar::String(s) => {
            w.u8(3);
            w.str(s);
        }
    }
}

pub(crate) fn read_scalar(r: &mut Reader) -> Result<Scalar> {
    Ok(match r.u8()? {
        0 => Scalar::Bool(r.u8()? != 0),
        1 => Scalar::I64(r.i64()?),
        2 => Scalar::F64(r.f64()?),
        3 => Scalar::String(r.str()?),
        t => return Err(Error::Codec(format!("bad scalar tag {t}"))),
    })
}

pub(crate) fn write_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Null => w.u8(0),
        Value::Bool(b) => {
            w.u8(1);
            w.u8(*b as u8);
        }
        Value::I64(i) => {
            w.u8(2);
            w.i64(*i);
        }
        Value::F64(f) => {
            w.u8(3);
            w.f64(*f);
        }
        Value::String(s) => {
            w.u8(4);
            w.str(s);
        }
        Value::Bytes(b) => {
            w.u8(5);
            w.bytes(b);
        }
        Value::List(items) => {
            w.u8(6);
            w.u32(items.len() as u32);
            for s in items {
                write_scalar(w, s);
            }
        }
    }
}

pub(crate) fn read_value(r: &mut Reader) -> Result<Value> {
    Ok(match r.u8()? {
        0 => Value::Null,
        1 => Value::Bool(r.u8()? != 0),
        2 => Value::I64(r.i64()?),
        3 => Value::F64(r.f64()?),
        4 => Value::String(r.str()?),
        5 => Value::Bytes(r.bytes()?),
        6 => {
            let n = r.u32()? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(read_scalar(r)?);
            }
            Value::List(items)
        }
        t => return Err(Error::Codec(format!("bad value tag {t}"))),
    })
}

pub(crate) fn write_properties(w: &mut Writer, p: &Properties) {
    w.u32(p.len() as u32);
    for (k, v) in p {
        // BTreeMap iterates in sorted key order → deterministic bytes.
        w.str(k);
        write_value(w, v);
    }
}

pub(crate) fn read_properties(r: &mut Reader) -> Result<Properties> {
    let n = r.u32()? as usize;
    let mut p = Properties::new();
    for _ in 0..n {
        let k = r.str()?;
        let v = read_value(r)?;
        p.insert(k, v);
    }
    Ok(p)
}

// ---- Records ----

pub(crate) fn write_node_record(w: &mut Writer, rec: &NodeRecord) {
    w.u32(rec.node_type);
    match &rec.embedding {
        None => w.u8(0),
        Some(emb) => {
            w.u8(1);
            w.u32(emb.len() as u32);
            for x in emb {
                w.f32(*x);
            }
        }
    }
    write_properties(w, &rec.properties);
}

pub(crate) fn read_node_record(r: &mut Reader) -> Result<NodeRecord> {
    let node_type = r.u32()?;
    let embedding = match r.u8()? {
        0 => None,
        1 => {
            let n = r.u32()? as usize;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(r.f32()?);
            }
            Some(v)
        }
        t => return Err(Error::Codec(format!("bad embedding tag {t}"))),
    };
    let properties = read_properties(r)?;
    Ok(NodeRecord {
        node_type,
        properties,
        embedding,
    })
}

pub(crate) fn write_edge_record(w: &mut Writer, rec: &EdgeRecord) {
    w.u64(rec.from);
    w.u64(rec.to);
    w.u32(rec.edge_type);
    w.f32(rec.weight);
    write_properties(w, &rec.properties);
}

pub(crate) fn read_edge_record(r: &mut Reader) -> Result<EdgeRecord> {
    Ok(EdgeRecord {
        from: r.u64()?,
        to: r.u64()?,
        edge_type: r.u32()?,
        weight: r.f32()?,
        properties: read_properties(r)?,
    })
}

/// CRC-32 (IEEE 802.3), computed without a table so there is no dependency and
/// the value is fixed across platforms. Used to detect a torn WAL record.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_round_trips_hostile_bit_patterns() {
        // Denormal, negative zero, and a long-decimal value must survive exact.
        let cases = [f32::from_bits(1), -0.0_f32, 0.1_f32, f32::MIN_POSITIVE];
        let mut w = Writer::default();
        for c in cases {
            w.f32(c);
        }
        let mut r = Reader::new(&w.buf);
        for c in cases {
            assert_eq!(r.f32().unwrap().to_bits(), c.to_bits());
        }
    }

    #[test]
    fn value_round_trip_including_bytes() {
        let vals = vec![
            Value::Null,
            Value::Bool(true),
            Value::I64(-42),
            Value::F64(3.5),
            Value::String("hi".into()),
            Value::Bytes(vec![0, 1, 2, 255]),
            Value::List(vec![Scalar::I64(1), Scalar::String("x".into())]),
        ];
        let mut w = Writer::default();
        for v in &vals {
            write_value(&mut w, v);
        }
        let mut r = Reader::new(&w.buf);
        for v in &vals {
            assert_eq!(&read_value(&mut r).unwrap(), v);
        }
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        let mut r = Reader::new(&[0u8, 1]); // too short for a u32
        assert!(r.u32().is_err());
    }
}
