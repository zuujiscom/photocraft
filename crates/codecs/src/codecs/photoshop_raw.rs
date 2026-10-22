//! Photoshop Raw (`.raw`): read files saved with Photoshop's retained header
//! (channel count, rows, columns, bit depth, interleave flag, header length).
//! Headerless streams still need explicit dimensions in File › Open, like Photoshop.

use crate::SampleType;
use crate::error::CodecError;
use crate::format::Format;
use crate::image::{ChannelLayout, Image};
use crate::options::Limits;

const F: Format = Format::PhotoshopRaw;
const MIN_HEADER: usize = 18;

fn bad(reason: impl std::fmt::Display) -> CodecError {
    CodecError::malformed(F, reason)
}

struct Header {
    channels: u16,
    rows: u32,
    cols: u32,
    depth: u16,
    interleaved: bool,
    header_size: usize,
    big_endian: bool,
}

fn read_u16(b: &[u8], off: usize, be: bool) -> u16 {
    if be { u16::from_be_bytes([b[off], b[off + 1]]) } else { u16::from_le_bytes([b[off], b[off + 1]]) }
}

fn read_u32(b: &[u8], off: usize, be: bool) -> u32 {
    if be { u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) } else { u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) }
}

fn layout_for_channels(channels: u16) -> Option<ChannelLayout> {
    match channels {
        1 => Some(ChannelLayout::Gray),
        2 => Some(ChannelLayout::GrayA),
        3 => Some(ChannelLayout::Rgb),
        4 => Some(ChannelLayout::Rgba),
        _ => None,
    }
}

fn payload_bytes(rows: u32, cols: u32, channels: u16, depth: u16) -> Option<u64> {
    let pixels = u64::from(rows).checked_mul(u64::from(cols))?;
    let ch = u64::from(channels);
    match depth {
        8 => pixels.checked_mul(ch),
        16 => pixels.checked_mul(ch)?.checked_mul(2),
        32 => pixels.checked_mul(ch)?.checked_mul(4),
        1 => {
            let bits = pixels.checked_mul(ch)?;
            Some(bits.div_ceil(8))
        }
        _ => None,
    }
}

fn try_parse_header(b: &[u8], be: bool) -> Option<Header> {
    if b.len() < MIN_HEADER {
        return None;
    }
    let channels = read_u16(b, 0, be);
    if !(1..=4).contains(&channels) {
        return None;
    }
    let rows = read_u32(b, 2, be);
    let cols = read_u32(b, 6, be);
    if rows == 0 || cols == 0 {
        return None;
    }
    let depth = read_u16(b, 10, be);
    if !matches!(depth, 8 | 16) {
        return None;
    }
    let mode = read_u16(b, 12, be);
    if mode > 1 {
        return None;
    }
    let header_size = read_u32(b, 14, be) as usize;
    if header_size < MIN_HEADER || header_size > b.len() {
        return None;
    }
    let payload = payload_bytes(rows, cols, channels, depth)?;
    if b.len() < header_size.saturating_add(payload as usize) {
        return None;
    }
    Some(Header { channels, rows, cols, depth, interleaved: mode == 0, header_size, big_endian: be })
}

/// True when `b` begins with a plausible Photoshop Raw retained header.
pub(crate) fn looks_like(b: &[u8]) -> bool {
    try_parse_header(b, true).is_some() || try_parse_header(b, false).is_some()
}

fn parse_header(b: &[u8]) -> Result<Header, CodecError> {
    try_parse_header(b, true)
        .or_else(|| try_parse_header(b, false))
        .ok_or_else(|| bad("missing or invalid Photoshop Raw header; headerless files need explicit width, height and channel count"))
}

fn interleave_planar_u8(data: &[u8], w: usize, h: usize, ch: usize) -> Result<Vec<u8>, CodecError> {
    let plane = w.checked_mul(h).ok_or_else(|| bad("image size overflow"))?;
    let need = plane.checked_mul(ch).ok_or_else(|| bad("image size overflow"))?;
    if data.len() < need {
        return Err(bad("truncated planar pixel data"));
    }
    let mut out = vec![0u8; need];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            for c in 0..ch {
                out[i * ch + c] = data[c * plane + i];
            }
        }
    }
    Ok(out)
}

fn read_u16_pixels(data: &[u8], w: usize, h: usize, ch: usize, interleaved: bool, be: bool) -> Result<Vec<u16>, CodecError> {
    let plane = w.checked_mul(h).ok_or_else(|| bad("image size overflow"))?;
    let samples = plane.checked_mul(ch).ok_or_else(|| bad("image size overflow"))?;
    let need = samples.checked_mul(2).ok_or_else(|| bad("image size overflow"))?;
    if data.len() < need {
        return Err(bad("truncated 16-bit pixel data"));
    }
    let mut raw = Vec::with_capacity(samples);
    if interleaved {
        let mut pos = 0;
        for _ in 0..samples {
            let v = if be { u16::from_be_bytes([data[pos], data[pos + 1]]) } else { u16::from_le_bytes([data[pos], data[pos + 1]]) };
            raw.push(v);
            pos += 2;
        }
    } else {
        for c in 0..ch {
            let base = c * plane * 2;
            for i in 0..plane {
                let pos = base + i * 2;
                let v = if be { u16::from_be_bytes([data[pos], data[pos + 1]]) } else { u16::from_le_bytes([data[pos], data[pos + 1]]) };
                raw.push(v);
            }
        }
        // planar read order is RRR..GGG..BBB..; interleave to RGBARGBA
        let mut interleaved = vec![0u16; samples];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                for c in 0..ch {
                    interleaved[i * ch + c] = raw[c * plane + i];
                }
            }
        }
        return Ok(interleaved);
    }
    Ok(raw)
}

pub(crate) fn decode(b: &[u8], limits: &Limits) -> Result<Image, CodecError> {
    let hdr = parse_header(b)?;
    let layout = layout_for_channels(hdr.channels).ok_or_else(|| bad("unsupported channel count"))?;
    limits.check(hdr.cols, hdr.rows, layout, if hdr.depth == 16 { SampleType::U16 } else { SampleType::U8 })?;
    let w = hdr.cols as usize;
    let h = hdr.rows as usize;
    let ch = hdr.channels as usize;
    let pixels = &b[hdr.header_size..];
    match hdr.depth {
        8 => {
            let need = w.checked_mul(h).and_then(|n| n.checked_mul(ch)).ok_or_else(|| bad("image size overflow"))?;
            let data = if hdr.interleaved {
                if pixels.len() < need {
                    return Err(bad("truncated interleaved pixel data"));
                }
                pixels[..need].to_vec()
            } else {
                interleave_planar_u8(pixels, w, h, ch)?
            };
            Image::from_u8(hdr.cols, hdr.rows, layout, data)
        }
        16 => {
            let values = read_u16_pixels(pixels, w, h, ch, hdr.interleaved, hdr.big_endian)?;
            Image::from_u16(hdr.cols, hdr.rows, layout, &values)
        }
        _ => Err(bad("unsupported bit depth")),
    }
}
