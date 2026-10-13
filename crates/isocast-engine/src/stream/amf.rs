//! AMF0, the value encoding RTMP commands and FLV metadata use (Adobe's
//! "Action Message Format -- AMF 0" specification).

use anyhow::{Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Amf {
    Number(f64),
    Boolean(bool),
    String(String),
    /// Properties in order.
    Object(Vec<(String, Amf)>),
    Null,
    Undefined,
    /// An associative array, written like an object with a count.
    EcmaArray(Vec<(String, Amf)>),
    StrictArray(Vec<Amf>),
}

const NUMBER: u8 = 0x00;
const BOOLEAN: u8 = 0x01;
const STRING: u8 = 0x02;
const OBJECT: u8 = 0x03;
const NULL: u8 = 0x05;
const UNDEFINED: u8 = 0x06;
const REFERENCE: u8 = 0x07;
const ECMA_ARRAY: u8 = 0x08;
const OBJECT_END: u8 = 0x09;
const STRICT_ARRAY: u8 = 0x0a;
const DATE: u8 = 0x0b;
const LONG_STRING: u8 = 0x0c;
const UNSUPPORTED: u8 = 0x0d;
const XML_DOCUMENT: u8 = 0x0f;
const TYPED_OBJECT: u8 = 0x10;

impl Amf {
    pub(crate) fn string(s: &str) -> Self {
        Self::String(s.to_string())
    }

    pub(crate) fn object(properties: &[(&str, Amf)]) -> Self {
        Self::Object(
            properties
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
        )
    }

    /// A property of an object or ECMA array.
    pub(crate) fn get(&self, key: &str) -> Option<&Amf> {
        match self {
            Self::Object(properties) | Self::EcmaArray(properties) => properties
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Self::Number(n) => {
                out.push(NUMBER);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Self::Boolean(b) => out.extend_from_slice(&[BOOLEAN, u8::from(*b)]),
            Self::String(s) if s.len() > usize::from(u16::MAX) => {
                out.push(LONG_STRING);
                out.extend_from_slice(&(s.len() as u32).to_be_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Self::String(s) => {
                out.push(STRING);
                write_name(s, out);
            }
            Self::Object(properties) => {
                out.push(OBJECT);
                write_properties(properties, out);
            }
            Self::Null => out.push(NULL),
            Self::Undefined => out.push(UNDEFINED),
            Self::EcmaArray(properties) => {
                out.push(ECMA_ARRAY);
                out.extend_from_slice(&(properties.len() as u32).to_be_bytes());
                write_properties(properties, out);
            }
            Self::StrictArray(values) => {
                out.push(STRICT_ARRAY);
                out.extend_from_slice(&(values.len() as u32).to_be_bytes());
                for value in values {
                    value.write(out);
                }
            }
        }
    }
}

/// A UTF-8 string with a 16-bit length, as property names are written.
fn write_name(name: &str, out: &mut Vec<u8>) {
    let bytes = &name.as_bytes()[..name.len().min(usize::from(u16::MAX))];
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn write_properties(properties: &[(String, Amf)], out: &mut Vec<u8>) {
    for (name, value) in properties {
        write_name(name, out);
        value.write(out);
    }
    out.extend_from_slice(&[0, 0, OBJECT_END]);
}

/// Values one after another, as a command or data message's body.
pub(crate) fn encode(values: &[Amf]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        value.write(&mut out);
    }
    out
}

/// Every value in a message body.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Amf>> {
    let mut reader = Reader { bytes, at: 0 };
    let mut values = Vec::new();
    while reader.at < bytes.len() {
        values.push(reader.value()?);
    }
    Ok(values)
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let Some(slice) = self.bytes.get(self.at..self.at + n) else {
            bail!("an AMF value runs past the end of its message");
        };
        self.at += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn string(&mut self, len: usize) -> Result<String> {
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }

    fn properties(&mut self) -> Result<Vec<(String, Amf)>> {
        let mut properties = Vec::new();
        loop {
            let len = usize::from(self.u16()?);
            if len == 0 && self.bytes.get(self.at) == Some(&OBJECT_END) {
                self.at += 1;
                return Ok(properties);
            }
            let name = self.string(len)?;
            properties.push((name, self.value()?));
        }
    }

    fn value(&mut self) -> Result<Amf> {
        Ok(match self.u8()? {
            NUMBER => Amf::Number(f64::from_be_bytes(self.take(8)?.try_into()?)),
            BOOLEAN => Amf::Boolean(self.u8()? != 0),
            STRING => {
                let len = usize::from(self.u16()?);
                Amf::String(self.string(len)?)
            }
            LONG_STRING | XML_DOCUMENT => {
                let len = self.u32()? as usize;
                Amf::String(self.string(len)?)
            }
            OBJECT => Amf::Object(self.properties()?),
            TYPED_OBJECT => {
                let len = usize::from(self.u16()?);
                self.take(len)?;
                Amf::Object(self.properties()?)
            }
            NULL => Amf::Null,
            UNDEFINED | UNSUPPORTED => Amf::Undefined,
            REFERENCE => {
                self.u16()?;
                Amf::Undefined
            }
            ECMA_ARRAY => {
                self.u32()?;
                Amf::EcmaArray(self.properties()?)
            }
            STRICT_ARRAY => {
                let count = self.u32()?;
                let mut values = Vec::new();
                for _ in 0..count {
                    values.push(self.value()?);
                }
                Amf::StrictArray(values)
            }
            DATE => {
                let ms = f64::from_be_bytes(self.take(8)?.try_into()?);
                self.u16()?;
                Amf::Number(ms)
            }
            marker => bail!("unknown AMF0 type {marker:#04x}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_in_the_spec_layout() {
        let values = vec![
            Amf::string("connect"),
            Amf::Number(1.0),
            Amf::object(&[
                ("app", Amf::string("live")),
                ("type", Amf::string("nonprivate")),
            ]),
            Amf::Null,
            Amf::Boolean(true),
            Amf::EcmaArray(vec![("width".into(), Amf::Number(1280.0))]),
            Amf::StrictArray(vec![Amf::Number(2.0), Amf::Undefined]),
        ];
        let bytes = encode(&values);
        // "connect": marker, 16-bit length, bytes.
        assert_eq!(&bytes[..10], b"\x02\x00\x07connect");
        // 1.0 as a big-endian double.
        assert_eq!(&bytes[10..19], &[0, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode(&bytes).unwrap(), values);
        let object = &decode(&bytes).unwrap()[2];
        assert_eq!(object.get("app").and_then(Amf::as_str), Some("live"));
    }

    #[test]
    fn truncated_values_are_errors() {
        let bytes = encode(&[Amf::string("onStatus")]);
        assert!(decode(&bytes[..bytes.len() - 1]).is_err());
    }
}
