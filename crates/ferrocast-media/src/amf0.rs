//! Minimal AMF0 encoder/decoder for RTMP commands and FLV metadata.

use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Value)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Value)>),
    StrictArray(Vec<Value>),
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::String(s.to_string())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Looks up a property of an object or ECMA array.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(props) | Value::EcmaArray(props) => {
                props.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }
}

fn write_key(out: &mut Vec<u8>, key: &str) {
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key.as_bytes());
}

fn write_props(out: &mut Vec<u8>, props: &[(String, Value)]) {
    for (k, v) in props {
        write_key(out, k);
        encode_into(out, v);
    }
    out.extend_from_slice(&[0, 0, 9]);
}

pub fn encode_into(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Number(n) => {
            out.push(0);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Boolean(b) => out.extend_from_slice(&[1, *b as u8]),
        Value::String(s) if s.len() > u16::MAX as usize => {
            out.push(0x0c);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::String(s) => {
            out.push(2);
            write_key(out, s);
        }
        Value::Object(props) => {
            out.push(3);
            write_props(out, props);
        }
        Value::Null => out.push(5),
        Value::Undefined => out.push(6),
        Value::EcmaArray(props) => {
            out.push(8);
            out.extend_from_slice(&(props.len() as u32).to_be_bytes());
            write_props(out, props);
        }
        Value::StrictArray(items) => {
            out.push(0x0a);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items {
                encode_into(out, item);
            }
        }
    }
}

pub fn encode(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for v in values {
        encode_into(&mut out, v);
    }
    out
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.data.len() < n {
            return Err(Error::Rtmp("truncated AMF0 value".into()));
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self, len: usize) -> Result<String> {
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn props(&mut self) -> Result<Vec<(String, Value)>> {
        let mut props = Vec::new();
        loop {
            let len = self.u16()? as usize;
            if len == 0 && self.data.first() == Some(&9) {
                self.take(1)?;
                return Ok(props);
            }
            let key = self.string(len)?;
            props.push((key, self.value()?));
        }
    }

    fn value(&mut self) -> Result<Value> {
        let marker = self.take(1)?[0];
        Ok(match marker {
            0 => Value::Number(f64::from_be_bytes(self.take(8)?.try_into().unwrap())),
            1 => Value::Boolean(self.take(1)?[0] != 0),
            2 => {
                let len = self.u16()? as usize;
                Value::String(self.string(len)?)
            }
            3 => Value::Object(self.props()?),
            5 => Value::Null,
            6 => Value::Undefined,
            8 => {
                self.u32()?;
                Value::EcmaArray(self.props()?)
            }
            0x0a => {
                let n = self.u32()?;
                let mut items = Vec::new();
                for _ in 0..n {
                    items.push(self.value()?);
                }
                Value::StrictArray(items)
            }
            0x0c => {
                let len = self.u32()? as usize;
                Value::String(self.string(len)?)
            }
            m => return Err(Error::Rtmp(format!("unsupported AMF0 marker {m:#x}"))),
        })
    }
}

/// Decodes a sequence of AMF0 values (e.g. a command message body).
pub fn decode_all(data: &[u8]) -> Result<Vec<Value>> {
    let mut r = Reader { data };
    let mut values = Vec::new();
    while !r.data.is_empty() {
        values.push(r.value()?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let values = vec![
            Value::str("connect"),
            Value::Number(1.0),
            Value::Object(vec![
                ("app".into(), Value::str("live")),
                ("fpad".into(), Value::Boolean(false)),
            ]),
            Value::Null,
            Value::EcmaArray(vec![("width".into(), Value::Number(1920.0))]),
            Value::StrictArray(vec![Value::Number(2.0), Value::Undefined]),
        ];
        let bytes = encode(&values);
        assert_eq!(decode_all(&bytes).unwrap(), values);
    }

    #[test]
    fn string_encoding() {
        assert_eq!(encode(&[Value::str("ab")]), vec![2, 0, 2, b'a', b'b']);
    }
}
