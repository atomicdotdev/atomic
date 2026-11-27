//! Experimental field-context columns. Replays the exact original postcard
//! bytes; it never combines graph operations or changes their algebra.
use anyhow::{ensure, Context, Result};
use serde::{ser, Serialize};
use std::{collections::HashMap, io::Read};

const MAGIC: &[u8; 5] = b"ATGL\x02";
const LIMIT: usize = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Error(String);
impl ser::Error for Error {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}
type SResult<T> = std::result::Result<T, Error>;

#[derive(Default, Serialize)]
struct Columns {
    control: Vec<u8>,
    numbers: Vec<Vec<u8>>,
    bytes: Vec<u8>,
    dictionary: Vec<Vec<u8>>,
    references: Vec<u8>,
    content_hash: Option<[u8; 32]>,
}
struct Borrowed<'a> {
    control: &'a [u8],
    numbers: Vec<&'a [u8]>,
    bytes: &'a [u8],
    dictionary: Vec<&'a [u8]>,
    references: &'a [u8],
    content_hash: Option<[u8; 32]>,
}

fn slice<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (bytes, rest) = postcard::take_from_bytes(input)?;
    *input = rest;
    Ok(bytes)
}
fn vector<'a>(input: &mut &'a [u8], limit: usize) -> Result<Vec<&'a [u8]>> {
    let n = usize::try_from(get(input)?)?;
    ensure!(n <= limit && n <= input.len(), "graph vector count limit");
    let mut values = Vec::new();
    values.try_reserve_exact(n)?;
    for _ in 0..n {
        values.push(slice(input)?);
    }
    Ok(values)
}
fn parse(mut input: &[u8]) -> Result<Borrowed<'_>> {
    let control = slice(&mut input)?;
    let numbers = vector(&mut input, 4096)?;
    let bytes = slice(&mut input)?;
    let dictionary = vector(&mut input, control.len().min(1_000_000))?;
    let references = slice(&mut input)?;
    let (content_hash, rest) = postcard::take_from_bytes(input)?;
    input = rest;
    ensure!(input.is_empty(), "trailing graph frame");
    Ok(Borrowed {
        control,
        numbers,
        bytes,
        dictionary,
        references,
        content_hash,
    })
}

fn put(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn get(input: &mut &[u8]) -> Result<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let (&b, rest) = input.split_first().context("truncated graph varint")?;
        *input = rest;
        ensure!(shift != 63 || b <= 1, "graph varint overflow");
        n |= u64::from(b & 127) << shift;
        if b < 128 {
            ensure!(shift == 0 || b != 0, "noncanonical graph varint");
            return Ok(n);
        }
    }
    anyhow::bail!("graph varint overflow")
}
fn delta(out: &mut Vec<u8>, previous: &mut u64, next: u64) {
    let d = next.wrapping_sub(*previous);
    put(out, (d << 1) ^ ((d as i64 >> 63) as u64));
    *previous = next;
}
fn undelta(input: &mut &[u8], previous: &mut u64) -> Result<u64> {
    let z = get(input)?;
    *previous = previous.wrapping_add((z >> 1) ^ 0u64.wrapping_sub(z & 1));
    Ok(*previous)
}

#[derive(Default)]
struct Writer {
    c: Columns,
    ctx: usize,
    paths: HashMap<(usize, &'static str), usize>,
    names: Vec<&'static str>,
    roles: HashMap<(usize, u8), usize>,
    previous: Vec<u64>,
    dictionary: HashMap<Vec<u8>, usize>,
    last_reference: u64,
    content_lookup: HashMap<Vec<u8>, usize>,
    authored: Vec<u8>,
    source_cursor: usize,
    sequential: bool,
}
impl Writer {
    fn enter(&mut self, name: &'static str) -> usize {
        let old = self.ctx;
        let next = self.paths.len() + 1;
        self.ctx = *self.paths.entry((old, name)).or_insert(next);
        if self.ctx == next {
            self.names.push(name);
        }
        old
    }
    fn column(&mut self, kind: u8, n: u64) -> SResult<usize> {
        let next = self.c.numbers.len();
        let id = *self.roles.entry((self.ctx, kind)).or_insert(next);
        if id == next {
            if next >= 4096 {
                return Err(Error("too many field columns".into()));
            }
            self.c.numbers.push(vec![]);
            self.previous.push(0);
        }
        delta(&mut self.c.numbers[id], &mut self.previous[id], n);
        Ok(id)
    }
    fn number(&mut self, kind: u8, n: u64) -> SResult<()> {
        let id = self.column(kind, n)?;
        self.c.control.push(16);
        put(&mut self.c.control, id as u64);
        Ok(())
    }
    fn byte_sequence(&mut self, bytes: &[u8]) -> SResult<()> {
        if self.sequential
            && self.c.content_hash.is_some()
            && self
                .authored
                .get(self.source_cursor..)
                .is_some_and(|rest| rest.starts_with(bytes))
        {
            let length = self.column(6, bytes.len() as u64)?;
            self.c.control.push(21);
            put(&mut self.c.control, length as u64);
            self.source_cursor += bytes.len();
            return Ok(());
        }
        if let Some(&offset) = self.content_lookup.get(bytes) {
            let a = self.column(5, offset as u64)?;
            let b = self.column(6, bytes.len() as u64)?;
            self.c.control.push(20);
            put(&mut self.c.control, a as u64);
            put(&mut self.c.control, b as u64);
            self.source_cursor = offset + bytes.len();
            Ok(())
        } else {
            self.reference(bytes, 18)
        }
    }
    fn small_number(&mut self, kind: u8, n: u64) -> SResult<()> {
        if n < 16 {
            self.c.control.push(n as u8);
            Ok(())
        } else {
            self.number(kind, n)
        }
    }
    fn reference(&mut self, bytes: &[u8], tag: u8) -> SResult<()> {
        let next = self.c.dictionary.len();
        let id = if let Some(&id) = self.dictionary.get(bytes) {
            id
        } else {
            self.dictionary.insert(bytes.to_vec(), next);
            self.c.dictionary.push(bytes.to_vec());
            next
        };
        self.c.control.push(tag);
        delta(&mut self.c.references, &mut self.last_reference, id as u64);
        Ok(())
    }
    fn raw<T: Serialize>(&mut self, value: &T) -> SResult<()> {
        let bytes = postcard::to_allocvec(value).map_err(|e| Error(e.to_string()))?;
        self.reference(&bytes, 19)
    }
    fn with<T: Serialize + ?Sized>(&mut self, name: &'static str, value: &T) -> SResult<()> {
        let old = self.enter(name);
        let result = value.serialize(&mut *self);
        self.ctx = old;
        result
    }
}

struct Compound<'a> {
    w: &'a mut Writer,
    parent: usize,
    pending: Option<(usize, Vec<u8>)>,
}
impl Compound<'_> {
    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        if let Some((len, bytes)) = &mut self.pending {
            let mut one = [0u8; 1];
            if postcard::to_slice(value, &mut one).is_ok_and(|bytes| bytes.len() == 1) {
                if bytes.len() >= *len {
                    return Err(Error("sequence exceeds declared length".into()));
                }
                bytes.push(one[0]);
                return Ok(());
            }
            let (len, bytes) = self.pending.take().unwrap();
            self.w.small_number(4, len as u64)?;
            for byte in bytes {
                ser::Serializer::serialize_u8(&mut *self.w, byte)?;
            }
        }
        value.serialize(&mut *self.w)
    }
    fn finish(self) -> SResult<()> {
        if let Some((len, bytes)) = self.pending {
            if len != bytes.len() {
                return Err(Error("sequence length mismatch".into()));
            }
            self.w.byte_sequence(&bytes)?;
        }
        self.w.ctx = self.parent;
        Ok(())
    }
}
macro_rules! sequence {
    ($trait:ident,$method:ident) => {
        impl ser::$trait for Compound<'_> {
            type Ok = ();
            type Error = Error;
            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
                self.element(value)
            }
            fn end(self) -> SResult<()> {
                self.finish()
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);
macro_rules! fields {
    ($trait:ident) => {
        impl ser::$trait for Compound<'_> {
            type Ok = ();
            type Error = Error;
            fn serialize_field<T: Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> SResult<()> {
                self.w.with(key, value)
            }
            fn end(self) -> SResult<()> {
                self.finish()
            }
        }
    };
}
fields!(SerializeStruct);
fields!(SerializeStructVariant);
impl ser::SerializeMap for Compound<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> SResult<()> {
        self.w.with("map_key", key)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> SResult<()> {
        self.w.with("map_value", value)
    }
    fn end(self) -> SResult<()> {
        self.finish()
    }
}

impl<'a> ser::Serializer for &'a mut Writer {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Compound<'a>;
    type SerializeTuple = Compound<'a>;
    type SerializeTupleStruct = Compound<'a>;
    type SerializeTupleVariant = Compound<'a>;
    type SerializeMap = Compound<'a>;
    type SerializeStruct = Compound<'a>;
    type SerializeStructVariant = Compound<'a>;
    fn is_human_readable(&self) -> bool {
        false
    }
    fn serialize_bool(self, v: bool) -> SResult<()> {
        self.c.control.push(u8::from(v));
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> SResult<()> {
        if v < 16 {
            self.c.control.push(v)
        } else {
            self.c.control.push(17);
            self.c.bytes.push(v);
        }
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> SResult<()> {
        self.number(0, v.into())
    }
    fn serialize_u32(self, v: u32) -> SResult<()> {
        self.number(1, v.into())
    }
    fn serialize_u64(self, v: u64) -> SResult<()> {
        self.number(2, v)
    }
    fn serialize_u128(self, v: u128) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_i8(self, v: i8) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_i16(self, v: i16) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_i32(self, v: i32) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_i64(self, v: i64) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_i128(self, v: i128) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_f32(self, v: f32) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_f64(self, v: f64) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_char(self, v: char) -> SResult<()> {
        self.raw(&v)
    }
    fn serialize_str(self, v: &str) -> SResult<()> {
        self.byte_sequence(v.as_bytes())
    }
    fn serialize_bytes(self, v: &[u8]) -> SResult<()> {
        self.byte_sequence(v)
    }
    fn serialize_none(self) -> SResult<()> {
        self.c.control.push(0);
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> SResult<()> {
        self.c.control.push(1);
        v.serialize(self)
    }
    fn serialize_unit(self) -> SResult<()> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> SResult<()> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, index: u32, _: &'static str) -> SResult<()> {
        self.small_number(3, index.into())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> SResult<()> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        index: u32,
        variant: &'static str,
        v: &T,
    ) -> SResult<()> {
        self.small_number(3, index.into())?;
        self.with(variant, v)
    }
    fn serialize_seq(self, len: Option<usize>) -> SResult<Compound<'a>> {
        let len = len.ok_or_else(|| Error("unknown sequence length".into()))?;
        let capture = self.c.content_hash.is_some()
            && self.ctx > 0
            && matches!(self.names[self.ctx - 1], "content" | "new_content");
        if capture {
            let parent = self.ctx;
            return Ok(Compound {
                w: self,
                parent,
                pending: Some((len, vec![])),
            });
        }
        self.small_number(4, len as u64)?;
        let parent = self.ctx;
        Ok(Compound {
            w: self,
            parent,
            pending: None,
        })
    }
    fn serialize_tuple(self, _: usize) -> SResult<Compound<'a>> {
        let parent = self.ctx;
        Ok(Compound {
            w: self,
            parent,
            pending: None,
        })
    }
    fn serialize_tuple_struct(self, _: &'static str, len: usize) -> SResult<Compound<'a>> {
        self.serialize_tuple(len)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        index: u32,
        variant: &'static str,
        _: usize,
    ) -> SResult<Compound<'a>> {
        self.small_number(3, index.into())?;
        let parent = self.enter(variant);
        Ok(Compound {
            w: self,
            parent,
            pending: None,
        })
    }
    fn serialize_map(self, len: Option<usize>) -> SResult<Compound<'a>> {
        self.small_number(
            4,
            len.ok_or_else(|| Error("unknown map length".into()))? as u64,
        )?;
        let parent = self.ctx;
        Ok(Compound {
            w: self,
            parent,
            pending: None,
        })
    }
    fn serialize_struct(self, _: &'static str, len: usize) -> SResult<Compound<'a>> {
        self.serialize_tuple(len)
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> SResult<Compound<'a>> {
        self.serialize_tuple_variant(name, index, variant, len)
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    encode_context(value, None, false)
}
pub fn encode_with_content<T: Serialize>(value: &T, content: &[u8]) -> Result<Vec<u8>> {
    encode_context(value, Some(content), false)
}
pub fn encode_with_sequential_content<T: Serialize>(value: &T, content: &[u8]) -> Result<Vec<u8>> {
    encode_context(value, Some(content), true)
}
fn encode_context<T: Serialize>(
    value: &T,
    content: Option<&[u8]>,
    sequential: bool,
) -> Result<Vec<u8>> {
    let mut writer = Writer {
        sequential,
        ..Writer::default()
    };
    if let Some(content) = content {
        ensure!(content.len() <= LIMIT, "authored lookup limit");
        writer.c.content_hash = Some(*blake3::hash(content).as_bytes());
        writer.authored = content.to_vec();
        let mut base = 0;
        for line in content.split_inclusive(|b| *b == b'\n') {
            writer.content_lookup.entry(line.to_vec()).or_insert(base);
            for token in atomic_core::diff::token::Tokenizer::new(line) {
                let at = base + token.offset();
                let key = token.content();
                if content.get(at..at + key.len()) == Some(key) {
                    writer.content_lookup.entry(key.to_vec()).or_insert(at);
                }
            }
            base += line.len();
        }
    }
    value.serialize(&mut writer)?;
    ensure!(
        writer.c.dictionary.len() <= 1_000_000,
        "graph dictionary limit"
    );
    let payload = postcard::to_allocvec(&writer.c)?;
    ensure!(payload.len() <= LIMIT * 2, "graph frame limit");
    let mut out = MAGIC.to_vec();
    out.extend(zstd::encode_all(payload.as_slice(), 3)?);
    Ok(out)
}
pub fn decode(encoded: &[u8]) -> Result<Vec<u8>> {
    decode_context(encoded, None)
}
pub fn decode_with_content(encoded: &[u8], content: &[u8]) -> Result<Vec<u8>> {
    decode_context(encoded, Some(content))
}
fn decode_context(encoded: &[u8], content: Option<&[u8]>) -> Result<Vec<u8>> {
    let data = encoded
        .strip_prefix(MAGIC)
        .context("unknown graph lab version")?;
    let mut raw = vec![];
    zstd::stream::read::Decoder::new(data)?
        .take((LIMIT * 2) as u64 + 1)
        .read_to_end(&mut raw)?;
    ensure!(raw.len() <= LIMIT * 2, "graph frame limit");
    let c = parse(&raw)?;
    replay(c, content, LIMIT)
}
fn numeric(c: &mut Borrowed<'_>, previous: &mut [u64], numbers_end: &mut usize) -> Result<u64> {
    let id = usize::try_from(get(&mut c.control)?)?;
    ensure!(id < c.numbers.len(), "graph numeric column index");
    ensure!(id <= *numbers_end, "graph numeric column gap");
    if id == *numbers_end {
        *numbers_end += 1;
    }
    undelta(&mut c.numbers[id], &mut previous[id])
}
fn replay(mut c: Borrowed<'_>, content: Option<&[u8]>, limit: usize) -> Result<Vec<u8>> {
    if let Some(hash) = c.content_hash {
        let bytes = content.context("authored content resolver required")?;
        ensure!(bytes.len() <= LIMIT, "authored content limit");
        ensure!(
            hash == *blake3::hash(bytes).as_bytes(),
            "authored content binding mismatch"
        );
    }
    ensure!(
        c.numbers.len() <= 4096 && c.dictionary.len() <= c.control.len(),
        "graph column/dictionary limit"
    );
    let mut previous = vec![0; c.numbers.len()];
    let mut reference = 0;
    let mut out = vec![];
    let mut dictionary_end = 0;
    let mut numbers_end = 0;
    let mut source_cursor = 0usize;
    while !c.control.is_empty() {
        let tag = c.control[0];
        c.control = &c.control[1..];
        match tag {
            0..=15 => out.push(tag),
            16 => {
                put(&mut out, numeric(&mut c, &mut previous, &mut numbers_end)?);
            }
            17 => {
                let (&byte, rest) = c
                    .bytes
                    .split_first()
                    .context("graph byte stream exhausted")?;
                c.bytes = rest;
                out.push(byte);
            }
            18 | 19 => {
                let id = usize::try_from(undelta(&mut c.references, &mut reference)?)?;
                let bytes = c.dictionary.get(id).context("graph dictionary index")?;
                ensure!(id <= dictionary_end, "graph dictionary gap");
                if id == dictionary_end {
                    dictionary_end += 1;
                }
                if tag == 18 {
                    put(&mut out, bytes.len() as u64);
                }
                ensure!(
                    out.len()
                        .checked_add(bytes.len())
                        .is_some_and(|n| n <= limit),
                    "graph expanded limit"
                );
                out.extend(*bytes);
            }
            20 => {
                ensure!(c.content_hash.is_some(), "unbound authored reference");
                let at = usize::try_from(numeric(&mut c, &mut previous, &mut numbers_end)?)?;
                let len = usize::try_from(numeric(&mut c, &mut previous, &mut numbers_end)?)?;
                let end = at.checked_add(len).context("authored range overflow")?;
                let bytes = content
                    .context("missing authored bytes")?
                    .get(at..end)
                    .context("authored range outside content")?;
                put(&mut out, len as u64);
                ensure!(
                    out.len().checked_add(len).is_some_and(|n| n <= limit),
                    "graph expanded limit"
                );
                out.extend(bytes);
                source_cursor = end;
            }
            21 => {
                ensure!(
                    c.content_hash.is_some(),
                    "unbound sequential authored reference"
                );
                let len = usize::try_from(numeric(&mut c, &mut previous, &mut numbers_end)?)?;
                let end = source_cursor
                    .checked_add(len)
                    .context("sequential range overflow")?;
                let bytes = content
                    .context("missing authored bytes")?
                    .get(source_cursor..end)
                    .context("sequential range outside content")?;
                put(&mut out, len as u64);
                ensure!(
                    out.len().checked_add(len).is_some_and(|n| n <= limit),
                    "graph expanded limit"
                );
                out.extend(bytes);
                source_cursor = end;
            }
            _ => anyhow::bail!("unknown graph event"),
        }
        ensure!(out.len() <= limit, "graph expanded limit");
    }
    ensure!(
        numbers_end == c.numbers.len()
            && dictionary_end == c.dictionary.len()
            && c.numbers.iter().all(|s| s.is_empty())
            && c.bytes.is_empty()
            && c.references.is_empty(),
        "unconsumed graph column"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Serialize)]
    enum Variant {
        Unit,
        New(u64),
        Tuple(u16, u32),
        Named { position: u32, text: String },
    }
    #[derive(Serialize)]
    struct Fixture {
        a: Vec<(u8, u16, u32, u64)>,
        b: Option<Vec<Variant>>,
        c: Option<u8>,
        signed: (i8, i16, i32, i64, i128),
        other: (u128, f32, f64, char, bool, (), [u8; 5]),
        map: std::collections::BTreeMap<String, u64>,
    }
    #[test]
    fn replays_exact_postcard_for_serde_shapes_and_numeric_extremes() -> Result<()> {
        let v = Fixture {
            a: vec![(0, 0, 0, 0), (255, u16::MAX, u32::MAX, u64::MAX)],
            b: Some(vec![
                Variant::Unit,
                Variant::New(1 << 63),
                Variant::Tuple(5, 7),
                Variant::Named {
                    position: 19,
                    text: "shared".into(),
                },
            ]),
            c: None,
            signed: (i8::MIN, i16::MIN, i32::MIN, i64::MIN, i128::MIN),
            other: (
                u128::MAX,
                f32::NAN,
                -0.0,
                'é',
                true,
                (),
                [0, 15, 16, 128, 255],
            ),
            map: [("shared".into(), 7), ("other".into(), u64::MAX)]
                .into_iter()
                .collect(),
        };
        let encoded = encode(&v)?;
        assert_eq!(decode(&encoded)?, postcard::to_allocvec(&v)?);
        assert_eq!(decode(&encode(&())?)?, postcard::to_allocvec(&())?);
        for end in 0..encoded.len() {
            assert!(decode(&encoded[..end]).is_err());
        }
        Ok(())
    }

    #[test]
    fn authored_byte_references_and_sequence_fallback_are_exact() -> Result<()> {
        #[derive(Serialize)]
        struct Bytes {
            content: Vec<u8>,
            new_content: Vec<Vec<u8>>,
            old: Vec<u8>,
            name: String,
        }
        let value = Bytes {
            content: b"hello".to_vec(),
            new_content: vec![vec![], vec![1, 2, 255]],
            old: b"absent old text".to_vec(),
            name: "prefix".into(),
        };
        let content = b"prefix hello\n";
        let encoded = encode_with_content(&value, content)?;
        assert_eq!(
            decode_with_content(&encoded, content)?,
            postcard::to_allocvec(&value)?
        );
        assert!(decode(&encoded).is_err());
        assert!(decode_with_content(&encoded, b"wrong bytes").is_err());
        #[derive(Serialize)]
        struct Fragment {
            content: Vec<u8>,
        }
        let pieces = [b"prefix".as_slice(), b"old", b"hello", b"\n", b" "]
            .into_iter()
            .map(|s| Fragment {
                content: s.to_vec(),
            })
            .collect::<Vec<_>>();
        let encoded = encode_with_sequential_content(&pieces, content)?;
        assert_eq!(
            decode_with_content(&encoded, content)?,
            postcard::to_allocvec(&pieces)?
        );
        assert_eq!(encoded, encode_with_sequential_content(&pieces, content)?);
        Ok(())
    }

    #[test]
    fn malformed_columns_and_amplification_fail_before_large_allocations() -> Result<()> {
        let packet = |c: &Columns| -> Result<Vec<u8>> {
            let mut encoded = MAGIC.to_vec();
            encoded.extend(zstd::encode_all(postcard::to_allocvec(c)?.as_slice(), 3)?);
            Ok(encoded)
        };
        for c in [
            Columns {
                control: vec![255],
                ..Default::default()
            },
            Columns {
                control: vec![16, 0],
                numbers: vec![vec![]],
                ..Default::default()
            },
            Columns {
                control: vec![18],
                references: vec![2],
                dictionary: vec![b"x".to_vec()],
                ..Default::default()
            },
            Columns {
                numbers: vec![vec![]],
                ..Default::default()
            },
        ] {
            assert!(decode(&packet(&c)?).is_err());
        }
        let c = Columns {
            control: vec![18],
            references: vec![0],
            dictionary: vec![vec![b'x'; 1024]],
            ..Default::default()
        };
        let bytes = postcard::to_allocvec(&c)?;
        assert!(replay(parse(&bytes)?, None, 64).is_err());
        let mut bytes = vec![0];
        put(&mut bytes, u64::MAX);
        assert!(parse(&bytes).is_err());
        Ok(())
    }
}
