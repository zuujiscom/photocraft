//! Photoshop Raw retained-header decode.

use photocraft_codecs::{ChannelLayout, Format, SampleType, decode, decode_as, detect, from_extension};

fn pack_header(ch: u16, rows: u32, cols: u32, depth: u16, interleaved: u16, pixels: &[u8]) -> Vec<u8> {
    let header_size = 18u32;
    let mut v = vec![0u8; header_size as usize + pixels.len()];
    v[0..2].copy_from_slice(&ch.to_be_bytes());
    v[2..6].copy_from_slice(&rows.to_be_bytes());
    v[6..10].copy_from_slice(&cols.to_be_bytes());
    v[10..12].copy_from_slice(&depth.to_be_bytes());
    v[12..14].copy_from_slice(&interleaved.to_be_bytes());
    v[14..18].copy_from_slice(&header_size.to_be_bytes());
    v[header_size as usize..].copy_from_slice(pixels);
    v
}

#[test]
fn detect_and_extension() {
    let bytes = pack_header(3, 1, 1, 8, 0, &[10, 20, 30]);
    assert_eq!(detect(&bytes), Some(Format::PhotoshopRaw));
    assert_eq!(from_extension("frame.raw"), Some(Format::PhotoshopRaw));
}

#[test]
fn decode_interleaved_rgb8() {
    let pixels: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    let bytes = pack_header(3, 2, 2, 8, 0, &pixels);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.dimensions(), (2, 2));
    assert_eq!(img.layout(), ChannelLayout::Rgb);
    assert_eq!(img.sample_type(), SampleType::U8);
    assert_eq!(img.data(), &pixels);
}

#[test]
fn decode_planar_rgb8() {
    let pixels: [u8; 12] = [1, 2, 3, 4, 10, 11, 12, 13, 20, 21, 22, 23];
    let bytes = pack_header(3, 2, 2, 8, 1, &pixels);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.data(), &[1, 10, 20, 2, 11, 21, 3, 12, 22, 4, 13, 23,]);
}

#[test]
fn decode_rgb16_big_endian() {
    let pixels = [0x00, 0x01, 0x01, 0x00, 0xFF, 0xFE, 0x00, 0x02, 0x02, 0x00, 0xFE, 0xFF];
    let bytes = pack_header(3, 1, 2, 16, 0, &pixels);
    let img = decode(&bytes).unwrap();
    assert_eq!(img.sample_type(), SampleType::U16);
    let mut vals = Vec::new();
    for chunk in img.data().as_chunks::<2>().0 {
        vals.push(u16::from_ne_bytes(*chunk));
    }
    assert_eq!(vals, [1, 256, 65534, 2, 512, 65279]);
}

#[test]
fn headerless_raw_errors() {
    let err = decode_as(Format::PhotoshopRaw, b"\0\0\0").unwrap_err();
    assert!(err.to_string().contains("header"));
}
