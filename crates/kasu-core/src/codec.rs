use crate::{Result, invalid};

// v1: big-endian integers; u32 length-prefixed byte strings and collections.
// Decoders reject unknown tags, trailing data, and noncanonical ordering.
#[derive(Default)]
pub struct Encoder(pub Vec<u8>);
impl Encoder {
    pub fn byte(&mut self, value: u8) {
        self.0.push(value);
    }
    pub fn u32(&mut self, value: u32) {
        self.0.extend(value.to_be_bytes());
    }
    pub fn u64(&mut self, value: u64) {
        self.0.extend(value.to_be_bytes());
    }
    pub fn i64(&mut self, value: i64) {
        self.0.extend(value.to_be_bytes());
    }
    pub fn raw(&mut self, value: &[u8]) {
        self.0.extend(value);
    }
    pub fn len(&mut self, value: usize) {
        self.u32(value.try_into().expect("canonical value exceeds u32::MAX"));
    }
    pub fn bytes(&mut self, value: &[u8]) {
        self.len(value.len());
        self.raw(value);
    }
}

pub struct Decoder<'a> {
    input: &'a [u8],
}
impl<'a> Decoder<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { input }
    }
    pub fn raw(&mut self, len: usize) -> Result<&'a [u8]> {
        if len > self.input.len() {
            return Err(invalid("truncated canonical value"));
        }
        let (value, rest) = self.input.split_at(len);
        self.input = rest;
        Ok(value)
    }
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.raw(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.raw(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.raw(8)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.raw(8)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        self.raw(len)
    }
    // Every collection item occupies at least one byte. Bound before allocating.
    pub fn count(&mut self) -> Result<usize> {
        let count = self.u32()? as usize;
        if count > self.input.len() {
            return Err(invalid("impossible collection size"));
        }
        Ok(count)
    }
    pub fn finish(self) -> Result<()> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(invalid("trailing canonical bytes"))
        }
    }
}

pub trait Canonical: Sized {
    fn encode_to(&self, out: &mut Encoder);
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self>;
    fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::default();
        self.encode_to(&mut out);
        out.0
    }
    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = Decoder::new(bytes);
        let value = Self::decode_from(&mut input)?;
        input.finish()?;
        Ok(value)
    }
}
impl Canonical for () {
    fn encode_to(&self, _: &mut Encoder) {}
    fn decode_from(_: &mut Decoder<'_>) -> Result<Self> {
        Ok(())
    }
}
impl Canonical for u64 {
    fn encode_to(&self, out: &mut Encoder) {
        out.u64(*self);
    }
    fn decode_from(input: &mut Decoder<'_>) -> Result<Self> {
        input.u64()
    }
}
