use kasu_core::{Result, invalid};
use std::io::{self, Write};
pub const MAX_PACKET: usize = 65520;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Packet<'a> {
    Data(&'a [u8]),
    Flush,
    Delimiter,
    ResponseEnd,
}
pub fn parse_packets(mut bytes: &[u8]) -> Result<Vec<Packet<'_>>> {
    let mut packets = Vec::new();
    while !bytes.is_empty() {
        if packets.len() >= 100_000 || bytes.len() < 4 {
            return Err(invalid("invalid or excessive pkt-line framing"));
        }
        let mut length = 0usize;
        for byte in &bytes[..4] {
            length = length * 16
                + match byte {
                    b'0'..=b'9' => (byte - b'0') as usize,
                    b'a'..=b'f' => (byte - b'a' + 10) as usize,
                    b'A'..=b'F' => (byte - b'A' + 10) as usize,
                    _ => return Err(invalid("invalid pkt-line length")),
                };
        }
        let packet = match length {
            0 => Packet::Flush,
            1 => Packet::Delimiter,
            2 => Packet::ResponseEnd,
            3 => return Err(invalid("reserved pkt-line length")),
            4..=MAX_PACKET => {
                if bytes.len() < length {
                    return Err(invalid("truncated pkt-line"));
                }
                Packet::Data(&bytes[4..length])
            }
            _ => return Err(invalid("pkt-line exceeds maximum length")),
        };
        bytes = &bytes[length.max(4)..];
        packets.push(packet);
    }
    Ok(packets)
}
pub fn write_packet(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_PACKET - 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pkt-line too large",
        ));
    }
    write!(out, "{:04x}", bytes.len() + 4)?;
    out.write_all(bytes)
}
pub struct Sideband<W>(pub W);
impl<W: Write> Write for Sideband<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(MAX_PACKET - 5) {
            write!(self.0, "{:04x}", chunk.len() + 5)?;
            self.0.write_all(&[1])?;
            self.0.write_all(chunk)?;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
