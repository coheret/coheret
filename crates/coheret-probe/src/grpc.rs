//! Just enough HTTP/2 to call grpc.health.v1.Health/Check over cleartext.
//!
//! We send a fixed HPACK header block (literals only, so no encoder state)
//! and one DATA frame with the HealthCheckRequest, then look for SERVING in
//! the response DATA frames. Response headers are never decoded, which saves
//! us a whole HPACK decoder.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

const HEALTH_CHECK_PATH: &str = "/grpc.health.v1.Health/Check";

/// The HTTP/2 "client magic" that opens every connection.
const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Give up reading after this many frames, so a chatty server can't keep us
/// busy until the deadline.
const MAX_FRAMES: usize = 128;

/// Health responses are tiny; anything bigger is suspicious.
const MAX_FRAME_PAYLOAD: usize = 1 << 20;

mod frame_type {
    pub const DATA: u8 = 0x0;
    pub const HEADERS: u8 = 0x1;
    pub const RST_STREAM: u8 = 0x3;
    pub const SETTINGS: u8 = 0x4;
    pub const PING: u8 = 0x6;
    pub const GOAWAY: u8 = 0x7;
}

mod flag {
    pub const END_STREAM: u8 = 0x1;
    pub const ACK: u8 = 0x1;
    pub const END_HEADERS: u8 = 0x4;
    pub const PADDED: u8 = 0x8;
}

/// Run one health check against `host:port`.
///
/// `Ok(true)` means SERVING, `Ok(false)` means the server answered with any
/// other status, `Err` is a transport/protocol problem. An empty `service`
/// checks the whole server.
pub fn health_check(
    host: &str,
    port: u16,
    service: Option<&str>,
    timeout: Duration,
) -> Result<bool, String> {
    let address = format!("{host}:{port}");
    let addr = address
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {address}: {e}"))?
        .next()
        .ok_or_else(|| format!("cannot resolve {address}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("connect to {address} failed: {e}"))?;
    let deadline = Instant::now() + timeout;

    let mut request = Vec::new();
    request.extend_from_slice(CONNECTION_PREFACE);
    request.extend_from_slice(&frame(frame_type::SETTINGS, 0, 0, &[]));
    request.extend_from_slice(&frame(
        frame_type::HEADERS,
        flag::END_HEADERS,
        1,
        &header_block(&address),
    ));
    request.extend_from_slice(&frame(
        frame_type::DATA,
        flag::END_STREAM,
        1,
        &request_message(service),
    ));
    write_all(&mut stream, &request, deadline)?;

    let mut body: Vec<u8> = Vec::new();
    let mut closed = false;
    for _ in 0..MAX_FRAMES {
        if closed || Instant::now() >= deadline {
            break;
        }
        let (ty, flags, stream_id, payload) = read_frame(&mut stream, deadline)?;
        if ty == frame_type::SETTINGS {
            if flags & flag::ACK == 0 {
                write_all(&mut stream, &frame(frame_type::SETTINGS, flag::ACK, 0, &[]), deadline)?;
            }
        } else if ty == frame_type::PING {
            if flags & flag::ACK == 0 {
                let ack = frame(frame_type::PING, flag::ACK, 0, &payload);
                write_all(&mut stream, &ack, deadline)?;
            }
        } else if ty == frame_type::DATA && stream_id == 1 {
            body.extend_from_slice(data_payload(flags, &payload));
            if let Some(serving) = parse_health_response(&body) {
                return Ok(serving);
            }
            if flags & flag::END_STREAM != 0 {
                closed = true;
            }
        } else if ty == frame_type::HEADERS && stream_id == 1 {
            // trailers-only response: grpc-status != 0 with no message
            if flags & flag::END_STREAM != 0 {
                closed = true;
            }
        } else if ty == frame_type::RST_STREAM && stream_id == 1 {
            return Err("stream reset by server".to_string());
        } else if ty == frame_type::GOAWAY {
            closed = true;
        }
    }

    match parse_health_response(&body) {
        Some(serving) => Ok(serving),
        None if Instant::now() >= deadline => {
            Err(format!("timed out after {}s", timeout.as_secs()))
        }
        None => Err("no health response received".to_string()),
    }
}

/// Strip the padding envelope off a DATA frame, if it has one.
fn data_payload(flags: u8, payload: &[u8]) -> &[u8] {
    if flags & flag::PADDED == 0 || payload.is_empty() {
        return payload;
    }
    let pad = payload[0] as usize;
    let end = payload.len().saturating_sub(pad);
    if end <= 1 {
        return &[];
    }
    &payload[1..end]
}

/// HTTP/2 frame: 24-bit length, type, flags, stream id, payload.
fn frame(ty: u8, flags: u8, stream_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(9 + len);
    out.push(((len >> 16) & 0xff) as u8);
    out.push(((len >> 8) & 0xff) as u8);
    out.push((len & 0xff) as u8);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&(stream_id & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// HPACK integer encoding (RFC 7541 §5.1).
fn encode_integer(value: usize, prefix_bits: u8, flags: u8, out: &mut Vec<u8>) {
    let max = (1usize << prefix_bits) - 1;
    if value < max {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max as u8);
    let mut rest = value - max;
    while rest >= 128 {
        out.push((rest % 128 + 128) as u8);
        rest /= 128;
    }
    out.push(rest as u8);
}

/// HPACK string literal, no Huffman coding.
fn encode_string(value: &str, out: &mut Vec<u8>) {
    encode_integer(value.len(), 7, 0x00, out);
    out.extend_from_slice(value.as_bytes());
}

/// The request headers. Only static-table indexes and literal-without-
/// indexing fields, so we never touch the dynamic table.
fn header_block(authority: &str) -> Vec<u8> {
    let mut block = Vec::new();
    block.push(0x83); // :method: POST (static index 3)
    block.push(0x86); // :scheme: http (static index 6)
    encode_integer(4, 4, 0x00, &mut block); // :path (static name index 4)
    encode_string(HEALTH_CHECK_PATH, &mut block);
    encode_integer(1, 4, 0x00, &mut block); // :authority (static name index 1)
    encode_string(authority, &mut block);
    block.push(0x00); // literal name follows
    encode_string("content-type", &mut block);
    encode_string("application/grpc", &mut block);
    block.push(0x00);
    encode_string("te", &mut block);
    encode_string("trailers", &mut block);
    block
}

/// gRPC message: 1 compression flag byte + 4-byte big-endian length +
/// protobuf-encoded HealthCheckRequest.
fn request_message(service: Option<&str>) -> Vec<u8> {
    let mut message = Vec::new();
    if let Some(service) = service.filter(|s| !s.is_empty()) {
        message.push(0x0a); // field 1 (service), length-delimited
        encode_varint(service.len() as u64, &mut message);
        message.extend_from_slice(service.as_bytes());
    }
    let mut out = Vec::with_capacity(5 + message.len());
    out.push(0); // not compressed
    out.extend_from_slice(&(message.len() as u32).to_be_bytes());
    out.extend_from_slice(&message);
    out
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// LEB128 varint; returns the value and how many bytes it took.
fn read_varint(buf: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut pos = start;
    loop {
        let byte = *buf.get(pos)?;
        value |= u64::from(byte & 0x7f) << shift;
        pos += 1;
        if byte & 0x80 == 0 {
            return Some((value, pos - start));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Pull the first complete gRPC message out of `body` and check whether its
/// HealthCheckResponse.status is SERVING (1). None = nothing complete yet.
fn parse_health_response(body: &[u8]) -> Option<bool> {
    if body.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    if body.len() < 5 + len {
        return None;
    }
    let message = &body[5..5 + len];
    let mut status = None;
    let mut pos = 0;
    while pos < message.len() {
        let (tag, n) = read_varint(message, pos)?;
        pos += n;
        let field = tag >> 3;
        let wire = tag & 0x07;
        match wire {
            0 => {
                let (value, n) = read_varint(message, pos)?;
                pos += n;
                if field == 1 {
                    status = Some(value);
                }
            }
            2 => {
                let (l, n) = read_varint(message, pos)?;
                pos = pos.checked_add(n)?.checked_add(l as usize)?;
                if pos > message.len() {
                    return None;
                }
            }
            5 => pos = bounded_skip(pos, 4, message.len())?,
            1 => pos = bounded_skip(pos, 8, message.len())?,
            _ => return None,
        }
    }
    status.map(|s| s == 1)
}

fn bounded_skip(pos: usize, n: usize, len: usize) -> Option<usize> {
    let next = pos.checked_add(n)?;
    if next > len {
        return None;
    }
    Some(next)
}

fn remaining(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "timed out".to_string())
}

fn read_exact(stream: &mut TcpStream, buf: &mut [u8], deadline: Instant) -> Result<(), String> {
    stream.set_read_timeout(Some(remaining(deadline)?)).map_err(|e| e.to_string())?;
    stream.read_exact(buf).map_err(|e| {
        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
            "timed out".to_string()
        } else {
            format!("read failed: {e}")
        }
    })
}

fn write_all(stream: &mut TcpStream, buf: &[u8], deadline: Instant) -> Result<(), String> {
    stream.set_write_timeout(Some(remaining(deadline)?)).map_err(|e| e.to_string())?;
    stream.write_all(buf).map_err(|e| {
        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) {
            "timed out".to_string()
        } else {
            format!("write failed: {e}")
        }
    })
}

fn read_frame(stream: &mut TcpStream, deadline: Instant) -> Result<(u8, u8, u32, Vec<u8>), String> {
    let mut header = [0u8; 9];
    read_exact(stream, &mut header, deadline)?;
    let length =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    if length > MAX_FRAME_PAYLOAD {
        return Err(format!("oversized frame ({length} bytes)"));
    }
    let ty = header[3];
    let flags = header[4];
    let stream_id = u32::from_be_bytes([header[5] & 0x7f, header[6], header[7], header[8]]);
    let mut payload = vec![0u8; length];
    read_exact(stream, &mut payload, deadline)?;
    Ok((ty, flags, stream_id, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_hpack_integers() {
        // the examples from RFC 7541 appendix C.1
        let mut out = Vec::new();
        encode_integer(10, 5, 0, &mut out);
        assert_eq!(out, vec![10]);

        let mut out = Vec::new();
        encode_integer(1337, 5, 0, &mut out);
        assert_eq!(out, vec![31, 154, 10]);

        let mut out = Vec::new();
        encode_integer(4, 4, 0, &mut out);
        assert_eq!(out, vec![4]);
    }

    #[test]
    fn encodes_strings_without_huffman() {
        let mut out = Vec::new();
        encode_string("te", &mut out);
        assert_eq!(out, vec![2, b't', b'e']);
    }

    #[test]
    fn builds_frame_headers() {
        let f = frame(frame_type::DATA, flag::END_STREAM, 1, &[1, 2, 3]);
        assert_eq!(&f[..9], [0u8, 0, 3, 0x0, 0x1, 0, 0, 0, 1].as_slice());
        assert_eq!(&f[9..], [1u8, 2, 3].as_slice());
    }

    #[test]
    fn builds_request_messages() {
        assert_eq!(request_message(None), vec![0, 0, 0, 0, 0]);
        assert_eq!(request_message(Some("")), vec![0, 0, 0, 0, 0]);
        assert_eq!(
            request_message(Some("ab")),
            vec![0, 0, 0, 0, 4, 0x0a, 2, b'a', b'b']
        );
    }

    #[test]
    fn parses_health_responses() {
        // SERVING
        let serving = [0, 0, 0, 0, 2, 0x08, 0x01];
        assert_eq!(parse_health_response(&serving), Some(true));
        // NOT_SERVING
        let not_serving = [0, 0, 0, 0, 2, 0x08, 0x02];
        assert_eq!(parse_health_response(&not_serving), Some(false));
        // incomplete
        assert_eq!(parse_health_response(&[0, 0, 0, 0, 2, 0x08]), None);
        // empty
        assert_eq!(parse_health_response(&[]), None);
    }

    #[test]
    fn reads_varints() {
        assert_eq!(read_varint(&[0x01], 0), Some((1, 1)));
        assert_eq!(read_varint(&[0xac, 0x02], 0), Some((300, 2)));
        assert_eq!(read_varint(&[], 0), None);
    }

    #[test]
    fn strips_data_frame_padding() {
        assert_eq!(data_payload(0, &[1, 2, 3]), [1u8, 2, 3].as_slice());
        // pad length 1, two payload bytes, one pad byte
        assert_eq!(data_payload(flag::PADDED, &[1, 9, 9, 0]), [9u8, 9].as_slice());
        assert_eq!(data_payload(flag::PADDED, &[5, 1]), &[] as &[u8]);
    }
}
