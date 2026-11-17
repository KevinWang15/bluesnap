//! Minimal, safe, spec-compliant BSON 1.1 (`https://bsonspec.org/spec.html`)
//! document encoder and decoder used by the `.bs` composition file format.

use std::fmt;

const TAG_DOUBLE: u8 = 0x01;
const TAG_STRING: u8 = 0x02;
const TAG_DOCUMENT: u8 = 0x03;
const TAG_ARRAY: u8 = 0x04;
const TAG_BINARY: u8 = 0x05;
const TAG_BOOLEAN: u8 = 0x08;
const TAG_NULL: u8 = 0x0A;
const TAG_INT32: u8 = 0x10;
const TAG_INT64: u8 = 0x12;

const MAX_DEPTH: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err(msg: impl Into<String>) -> Error {
    Error(msg.into())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Double(f64),
    String(String),
    Document(Doc),
    Array(Vec<Value>),
    Binary(Vec<u8>),
    Bool(bool),
    Null,
    Int32(i32),
    Int64(i64),
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Double(v) => Some(v),
            Value::Int32(v) => Some(v as f64),
            Value::Int64(v) => Some(v as f64),
            _ => None,
        }
    }

    pub fn as_f32(&self) -> Option<f32> {
        let v = self.as_f64()? as f32;
        v.is_finite().then_some(v)
    }

    pub fn as_i32(&self) -> Option<i32> {
        match *self {
            Value::Int32(v) => Some(v),
            Value::Int64(v) => i32::try_from(v).ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int64(v) => Some(v),
            Value::Int32(v) => Some(v as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_doc(&self) -> Option<&Doc> {
        match self {
            Value::Document(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Binary(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }
}

/// Ordered BSON document (`Vec<(String, Value)>` preserves field order cleanly).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doc {
    fields: Vec<(String, Value)>,
}

impl Doc {
    pub fn new() -> Self {
        Self { fields: Vec::new() }
    }

    pub fn push(&mut self, key: impl Into<String>, value: Value) -> &mut Self {
        self.fields.push((key.into(), value));
        self
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn req_str(&self, key: &str) -> Result<&str, Error> {
        self.get(key).and_then(Value::as_str).ok_or_else(|| err(format!("missing or invalid string field `{key}`")))
    }

    pub fn req_i32(&self, key: &str) -> Result<i32, Error> {
        self.get(key).and_then(Value::as_i32).ok_or_else(|| err(format!("missing or invalid int32 field `{key}`")))
    }

    pub fn req_i64(&self, key: &str) -> Result<i64, Error> {
        self.get(key).and_then(Value::as_i64).ok_or_else(|| err(format!("missing or invalid int64 field `{key}`")))
    }

    pub fn req_f32(&self, key: &str) -> Result<f32, Error> {
        self.get(key).and_then(Value::as_f32).ok_or_else(|| err(format!("missing or invalid number field `{key}`")))
    }

    pub fn req_doc(&self, key: &str) -> Result<&Doc, Error> {
        self.get(key).and_then(Value::as_doc).ok_or_else(|| err(format!("missing or invalid document field `{key}`")))
    }

    pub fn req_array(&self, key: &str) -> Result<&[Value], Error> {
        self.get(key).and_then(Value::as_array).ok_or_else(|| err(format!("missing or invalid array field `{key}`")))
    }

    pub fn req_bytes(&self, key: &str) -> Result<&[u8], Error> {
        self.get(key).and_then(Value::as_bytes).ok_or_else(|| err(format!("missing or invalid binary field `{key}`")))
    }
}

pub fn encode(doc: &Doc) -> Result<Vec<u8>, Error> {
    let mut buf = Vec::new();
    write_doc(&mut buf, doc, 0)?;
    Ok(buf)
}

pub fn decode(bytes: &[u8]) -> Result<Doc, Error> {
    let (doc, rest) = read_doc(bytes, 0)?;
    if !rest.is_empty() {
        return Err(err("trailing bytes after BSON document"));
    }
    Ok(doc)
}

fn write_doc(buf: &mut Vec<u8>, doc: &Doc, depth: usize) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(err("BSON nesting exceeds maximum depth"));
    }
    let start = buf.len();
    buf.extend_from_slice(&[0; 4]);
    for (key, value) in &doc.fields {
        write_elem(buf, key, value, depth)?;
    }
    buf.push(0);
    let len = i32::try_from(buf.len() - start).map_err(|_| err("BSON document exceeds 2 GiB"))?;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
    Ok(())
}

fn write_cstring(buf: &mut Vec<u8>, s: &str) -> Result<(), Error> {
    if s.as_bytes().contains(&0) {
        return Err(err("BSON field name contains NUL byte"));
    }
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
    Ok(())
}

fn write_elem(buf: &mut Vec<u8>, key: &str, value: &Value, depth: usize) -> Result<(), Error> {
    match value {
        Value::Double(v) => {
            buf.push(TAG_DOUBLE);
            write_cstring(buf, key)?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::String(s) => {
            buf.push(TAG_STRING);
            write_cstring(buf, key)?;
            let len = i32::try_from(s.len().saturating_add(1)).map_err(|_| err("BSON string too long"))?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
            buf.push(0);
        }
        Value::Document(d) => {
            buf.push(TAG_DOCUMENT);
            write_cstring(buf, key)?;
            write_doc(buf, d, depth + 1)?;
        }
        Value::Array(items) => {
            buf.push(TAG_ARRAY);
            write_cstring(buf, key)?;
            if depth + 1 > MAX_DEPTH {
                return Err(err("BSON nesting exceeds maximum depth"));
            }
            let start = buf.len();
            buf.extend_from_slice(&[0; 4]);
            for (i, item) in items.iter().enumerate() {
                write_elem(buf, &i.to_string(), item, depth + 1)?;
            }
            buf.push(0);
            let len = i32::try_from(buf.len() - start).map_err(|_| err("BSON array exceeds 2 GiB"))?;
            buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
        }
        Value::Binary(bytes) => {
            buf.push(TAG_BINARY);
            write_cstring(buf, key)?;
            let len = i32::try_from(bytes.len()).map_err(|_| err("BSON binary exceeds 2 GiB"))?;
            buf.extend_from_slice(&len.to_le_bytes());
            buf.push(0x00); // generic binary subtype
            buf.extend_from_slice(bytes);
        }
        Value::Bool(b) => {
            buf.push(TAG_BOOLEAN);
            write_cstring(buf, key)?;
            buf.push(u8::from(*b));
        }
        Value::Null => {
            buf.push(TAG_NULL);
            write_cstring(buf, key)?;
        }
        Value::Int32(v) => {
            buf.push(TAG_INT32);
            write_cstring(buf, key)?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Value::Int64(v) => {
            buf.push(TAG_INT64);
            write_cstring(buf, key)?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    Ok(())
}

fn read_i32(input: &[u8]) -> Result<(i32, &[u8]), Error> {
    let (head, rest) = input.split_at_checked(4).ok_or_else(|| err("unexpected EOF reading int32"))?;
    Ok((i32::from_le_bytes(head.try_into().unwrap()), rest))
}

fn read_cstring(input: &[u8]) -> Result<(&str, &[u8]), Error> {
    let nul = input.iter().position(|&b| b == 0).ok_or_else(|| err("unterminated BSON cstring"))?;
    let s = std::str::from_utf8(&input[..nul]).map_err(|_| err("invalid UTF-8 in BSON cstring"))?;
    Ok((s, &input[nul + 1..]))
}

fn read_doc(input: &[u8], depth: usize) -> Result<(Doc, &[u8]), Error> {
    if depth > MAX_DEPTH {
        return Err(err("BSON nesting exceeds maximum depth"));
    }
    let (total_len, _) = read_i32(input)?;
    let total = usize::try_from(total_len).ok().filter(|&n| n >= 5).ok_or_else(|| err("invalid BSON document length"))?;
    let (doc_bytes, outer_rest) = input.split_at_checked(total).ok_or_else(|| err("truncated BSON document"))?;
    if *doc_bytes.last().unwrap() != 0 {
        return Err(err("BSON document missing trailing NUL"));
    }
    let mut cur = &doc_bytes[4..total - 1];
    let mut doc = Doc::new();
    while !cur.is_empty() {
        let tag = cur[0];
        let (key, after_key) = read_cstring(&cur[1..])?;
        let (val, next) = read_val(tag, after_key, depth)?;
        doc.push(key, val);
        cur = next;
    }
    Ok((doc, outer_rest))
}

fn read_val(tag: u8, input: &[u8], depth: usize) -> Result<(Value, &[u8]), Error> {
    match tag {
        TAG_DOUBLE => {
            let (head, rest) = input.split_at_checked(8).ok_or_else(|| err("unexpected EOF reading double"))?;
            Ok((Value::Double(f64::from_le_bytes(head.try_into().unwrap())), rest))
        }
        TAG_STRING => {
            let (len, rest) = read_i32(input)?;
            let n = usize::try_from(len).ok().filter(|&n| n >= 1).ok_or_else(|| err("invalid BSON string length"))?;
            let (raw, rest) = rest.split_at_checked(n).ok_or_else(|| err("truncated BSON string"))?;
            if *raw.last().unwrap() != 0 {
                return Err(err("BSON string missing trailing NUL"));
            }
            let s = std::str::from_utf8(&raw[..n - 1]).map_err(|_| err("invalid UTF-8 in BSON string"))?;
            Ok((Value::String(s.to_owned()), rest))
        }
        TAG_DOCUMENT => {
            let (d, rest) = read_doc(input, depth + 1)?;
            Ok((Value::Document(d), rest))
        }
        TAG_ARRAY => {
            let (d, rest) = read_doc(input, depth + 1)?;
            let items = d.fields.into_iter().map(|(_, v)| v).collect();
            Ok((Value::Array(items), rest))
        }
        TAG_BINARY => {
            let (len, rest) = read_i32(input)?;
            let n = usize::try_from(len).map_err(|_| err("negative BSON binary length"))?;
            let (&subtype, rest) = rest.split_first().ok_or_else(|| err("truncated BSON binary subtype"))?;
            let (payload, rest) = rest.split_at_checked(n).ok_or_else(|| err("truncated BSON binary payload"))?;
            // Subtype 0x02 (deprecated old-binary) prefixes an extra 4-byte length.
            let bytes = if subtype == 0x02 {
                let (inner_len, inner) = read_i32(payload)?;
                let m = usize::try_from(inner_len).ok().filter(|&m| m == inner.len()).ok_or_else(|| err("bad BSON binary(2) length"))?;
                inner[..m].to_vec()
            } else {
                payload.to_vec()
            };
            Ok((Value::Binary(bytes), rest))
        }
        TAG_BOOLEAN => {
            let (&b, rest) = input.split_first().ok_or_else(|| err("truncated BSON boolean"))?;
            match b {
                0 => Ok((Value::Bool(false), rest)),
                1 => Ok((Value::Bool(true), rest)),
                _ => Err(err("invalid BSON boolean byte")),
            }
        }
        TAG_NULL => Ok((Value::Null, input)),
        TAG_INT32 => {
            let (v, rest) = read_i32(input)?;
            Ok((Value::Int32(v), rest))
        }
        TAG_INT64 => {
            let (head, rest) = input.split_at_checked(8).ok_or_else(|| err("unexpected EOF reading int64"))?;
            Ok((Value::Int64(i64::from_le_bytes(head.try_into().unwrap())), rest))
        }
        other => Err(err(format!("unsupported BSON element tag 0x{other:02x}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bson_spec_hello_world_vector_matches_exact_bytes() {
        // Official BSON 1.1 specification example: {"hello": "world"}
        let mut doc = Doc::new();
        doc.push("hello", Value::String("world".into()));
        let bytes = encode(&doc).unwrap();
        let expected = b"\x16\x00\x00\x00\x02hello\x00\x06\x00\x00\x00world\x00\x00";
        assert_eq!(bytes, expected);
        assert_eq!(decode(&bytes).unwrap(), doc);
    }

    #[test]
    fn bson_round_trips_nested_documents_arrays_and_binary() {
        let mut inner = Doc::new();
        inner.push("flag", Value::Bool(true));
        inner.push("null", Value::Null);
        let mut root = Doc::new();
        root.push("pi", Value::Double(3.5));
        root.push("i32", Value::Int32(-42));
        root.push("i64", Value::Int64(9_000_000_000));
        root.push("bin", Value::Binary(vec![0, 1, 2, 255]));
        root.push("arr", Value::Array(vec![Value::Int32(1), Value::Document(inner)]));

        let encoded = encode(&root).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, root);
    }

    #[test]
    fn bson_rejects_truncated_and_malformed_inputs() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[5, 0, 0, 0, 1]).is_err()); // missing trailing NUL
        assert!(decode(&[10, 0, 0, 0, 0]).is_err()); // truncated length
        assert!(decode(&[5, 0, 0, 0, 0, 99]).is_err()); // trailing garbage
    }
}
