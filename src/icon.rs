//! Factory's icon, drawn in code: a factory silhouette on an orange rounded square.
//!
//! The app renders it for its window, and `build.rs` compiles this same file to produce the icon
//! embedded in the exe and the ones the MSI, the macOS app and the Linux packages use. Sizes are in
//! pixels and images are top-down, unpremultiplied RGBA.

/// The sizes in the `.ico` and in the exe, largest first.
pub const ICON_SIZES: [u32; 8] = [256, 64, 48, 40, 32, 24, 20, 16];
/// The size of the Linux packages' `.png`.
pub const PNG_SIZE: u32 = 256;
/// The entries in the `.icns`: each one's type code and the size of its PNG.
pub const ICNS_ENTRIES: [([u8; 4], u32); 4] = [(*b"ic07", 128), (*b"ic08", 256), (*b"ic09", 512), (*b"ic10", 1024)];

const SUPERSAMPLE: u32 = 4;
/// Below this the windows are mush, so the smallest sizes are the bare silhouette.
const MIN_SIZE_WITH_WINDOWS: u32 = 32;

const CLEAR: [u8; 4] = [0, 0, 0, 0];
const BACKGROUND: [u8; 4] = [232, 89, 12, 255];
const BUILDING: [u8; 4] = [255, 248, 240, 255];

// All geometry is in a unit square, with y pointing down.
const TILE_MIN: f64 = 0.04;
const TILE_MAX: f64 = 0.96;
const TILE_RADIUS: f64 = 0.2;
const GROUND: f64 = 0.8;
const CHIMNEY_LEFT: f64 = 0.18;
const CHIMNEY_RIGHT: f64 = 0.31;
const CHIMNEY_TOP: f64 = 0.2;
const ROOF_LEFT: f64 = 0.31;
const ROOF_RIGHT: f64 = 0.82;
const ROOF_TEETH: f64 = 3.0;
const ROOF_LOW: f64 = 0.6;
const ROOF_HIGH: f64 = 0.42;
const WINDOW_TOP: f64 = 0.65;
const WINDOW_BOTTOM: f64 = 0.73;
const WINDOW_WIDTH: f64 = 0.075;

fn in_tile(x: f64, y: f64) -> bool {
    // Distance to the inner square that the rounded corners curve around.
    let centre = (TILE_MIN + TILE_MAX) / 2.0;
    let half = (TILE_MAX - TILE_MIN) / 2.0 - TILE_RADIUS;
    let dx = ((x - centre).abs() - half).max(0.0);
    let dy = ((y - centre).abs() - half).max(0.0);
    dx.hypot(dy) <= TILE_RADIUS
}

fn in_building(x: f64, y: f64, windows: bool) -> bool {
    if y > GROUND {
        return false;
    }
    if (CHIMNEY_LEFT..=CHIMNEY_RIGHT).contains(&x) && y >= CHIMNEY_TOP {
        return true;
    }
    if !(ROOF_LEFT..ROOF_RIGHT).contains(&x) {
        return false;
    }
    // Each tooth of the sawtooth roof rises to the right, then drops straight down.
    let tooth_width = (ROOF_RIGHT - ROOF_LEFT) / ROOF_TEETH;
    let position = (x - ROOF_LEFT) / tooth_width;
    if y < ROOF_LOW + (ROOF_HIGH - ROOF_LOW) * position.fract() {
        return false;
    }
    let window_centre = ROOF_LEFT + (position.floor() + 0.5) * tooth_width;
    let in_window = (WINDOW_TOP..=WINDOW_BOTTOM).contains(&y)
        && (x - window_centre).abs() <= WINDOW_WIDTH / 2.0;
    !(windows && in_window)
}

fn sample(x: f64, y: f64, windows: bool) -> [u8; 4] {
    if !in_tile(x, y) {
        CLEAR
    } else if in_building(x, y, windows) {
        BUILDING
    } else {
        BACKGROUND
    }
}

/// Draws the icon at `size` by `size`, antialiased by averaging several samples per pixel.
pub fn render(size: u32) -> Vec<u8> {
    let windows = size >= MIN_SIZE_WITH_WINDOWS;
    let samples = SUPERSAMPLE * SUPERSAMPLE;
    let offset = |step: u32| (f64::from(step) + 0.5) / f64::from(SUPERSAMPLE);
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for row in 0..size {
        for col in 0..size {
            let mut alpha = 0u32;
            let mut channels = [0u32; 3];
            for sub_row in 0..SUPERSAMPLE {
                let y = (f64::from(row) + offset(sub_row)) / f64::from(size);
                for sub_col in 0..SUPERSAMPLE {
                    let x = (f64::from(col) + offset(sub_col)) / f64::from(size);
                    let [red, green, blue, a] = sample(x, y, windows).map(u32::from);
                    // Weighting by alpha stops clear samples darkening the edges.
                    alpha += a;
                    channels[0] += red * a;
                    channels[1] += green * a;
                    channels[2] += blue * a;
                }
            }
            if alpha == 0 {
                rgba.extend(CLEAR);
            } else {
                rgba.extend(channels.map(|channel| ((channel + alpha / 2) / alpha) as u8));
                rgba.push(((alpha + samples / 2) / samples) as u8);
            }
        }
    }
    rgba
}

/// Encodes an image the way icon files store it: bottom-up BGRA followed by a 1-bit AND mask.
pub fn encode_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let side = size as usize;
    let rows = || rgba.chunks_exact(side * 4).rev();
    let mask_stride = side.div_ceil(32) * 4;
    let image_size = side * side * 4 + side * mask_stride;

    let mut dib = Vec::with_capacity(40 + image_size);
    // BITMAPINFOHEADER. The height is doubled to cover both the colours and the mask.
    for field in [40, size, size * 2] {
        dib.extend(u32::to_le_bytes(field));
    }
    dib.extend(1u16.to_le_bytes());
    dib.extend(32u16.to_le_bytes());
    for field in [0, image_size as u32, 0, 0, 0, 0] {
        dib.extend(u32::to_le_bytes(field));
    }

    for row in rows() {
        for pixel in row.chunks_exact(4) {
            dib.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    for row in rows() {
        let mut bits = vec![0u8; mask_stride];
        for (col, pixel) in row.chunks_exact(4).enumerate() {
            if pixel[3] == 0 {
                bits[col / 8] |= 0x80u8 >> (col % 8);
            }
        }
        dib.extend(bits);
    }
    dib
}

fn directory_header(count: usize) -> Vec<u8> {
    let mut header = Vec::new();
    for field in [0, 1, count as u16] {
        header.extend(u16::to_le_bytes(field));
    }
    header
}

/// The part of a directory entry that `.ico` files and icon resources share.
fn directory_entry(size: u32, data_len: usize) -> Vec<u8> {
    // A width or height of 0 means 256.
    let side = (size % 256) as u8;
    let mut entry = vec![side, side, 0, 0];
    entry.extend(1u16.to_le_bytes());
    entry.extend(32u16.to_le_bytes());
    entry.extend((data_len as u32).to_le_bytes());
    entry
}

/// Builds a `.ico` file from (size, DIB) pairs.
pub fn encode_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut ico = directory_header(images.len());
    let mut offset = ico.len() + 16 * images.len();
    for (size, data) in images {
        ico.extend(directory_entry(*size, data.len()));
        ico.extend((offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in images {
        ico.extend(data);
    }
    ico
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const GROUP_ID: u16 = 1;

fn push_resource(res: &mut Vec<u8>, kind: u16, id: u16, data: &[u8]) {
    // Moveable and discardable, in US English, except in the empty record that opens the file.
    let (memory_flags, language) = if kind == 0 {
        (0u16, 0u16)
    } else {
        (0x1010, 0x0409)
    };
    res.extend((data.len() as u32).to_le_bytes());
    res.extend(32u32.to_le_bytes());
    for field in [0xFFFF, kind, 0xFFFF, id] {
        res.extend(u16::to_le_bytes(field));
    }
    res.extend(0u32.to_le_bytes());
    res.extend(memory_flags.to_le_bytes());
    res.extend(language.to_le_bytes());
    res.extend(0u32.to_le_bytes());
    res.extend(0u32.to_le_bytes());
    res.extend(data);
    res.resize(res.len().next_multiple_of(4), 0);
}

/// Builds a compiled resource file from (size, DIB) pairs, which the MSVC linker accepts directly.
/// This avoids needing `rc.exe` at build time. Explorer shows the exe's first icon group.
pub fn encode_res(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut res = Vec::new();
    push_resource(&mut res, 0, 0, &[]);
    let mut group = directory_header(images.len());
    for (id, (size, data)) in (1u16..).zip(images) {
        push_resource(&mut res, RT_ICON, id, data);
        group.extend(directory_entry(*size, data.len()));
        group.extend(id.to_le_bytes());
    }
    push_resource(&mut res, RT_GROUP_ICON, GROUP_ID, &group);
    res
}

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
/// The most a stored deflate block can hold.
const MAX_STORED_BLOCK: usize = 0xFFFF;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// A zlib stream of stored deflate blocks, which is valid without compressing anything.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut stream: Vec<u8> = vec![0x78, 0x01];
    let mut blocks = data.chunks(MAX_STORED_BLOCK).peekable();
    if blocks.peek().is_none() {
        stream.extend([1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = blocks.next() {
        let is_last = blocks.peek().is_none();
        let len = block.len() as u16;
        stream.push(u8::from(is_last));
        stream.extend(len.to_le_bytes());
        stream.extend((!len).to_le_bytes());
        stream.extend(block);
    }
    stream.extend(adler32(data).to_be_bytes());
    stream
}

fn push_png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend((data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend(kind);
    png.extend(data);
    let crc = crc32(&png[start..]);
    png.extend(crc.to_be_bytes());
}

/// Encodes an image as an 8-bit RGBA PNG. It isn't compressed, which keeps this free of dependencies.
pub fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut header = Vec::with_capacity(13);
    header.extend(size.to_be_bytes());
    header.extend(size.to_be_bytes());
    // 8 bits per channel, RGBA, then the only compression and filter methods, and no interlacing.
    header.extend([8, 6, 0, 0, 0]);

    let stride = size as usize * 4;
    let mut filtered: Vec<u8> = Vec::with_capacity(rgba.len() + size as usize);
    for row in rgba.chunks_exact(stride) {
        // Each row starts with its filter type, and 0 means unfiltered.
        filtered.push(0);
        filtered.extend(row);
    }

    let mut png = PNG_SIGNATURE.to_vec();
    push_png_chunk(&mut png, b"IHDR", &header);
    push_png_chunk(&mut png, b"IDAT", &zlib_stored(&filtered));
    push_png_chunk(&mut png, b"IEND", &[]);
    png
}

/// Builds a `.icns` file from (type code, PNG) pairs. Its lengths are big-endian and include the
/// 8-byte header of whatever they measure.
pub fn encode_icns(entries: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let total = 8 + entries.iter().map(|(_, data)| 8 + data.len()).sum::<usize>();
    let mut icns = Vec::with_capacity(total);
    icns.extend(b"icns");
    icns.extend((total as u32).to_be_bytes());
    for (kind, data) in entries {
        icns.extend(kind);
        icns.extend(((8 + data.len()) as u32).to_be_bytes());
        icns.extend(data);
    }
    icns
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_be_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// Reads a zlib stream of stored blocks back, checking each block's length and the checksum.
    fn inflate_stored(stream: &[u8]) -> Vec<u8> {
        assert_eq!(u16::from_be_bytes([stream[0], stream[1]]) % 31, 0, "zlib header check");
        let mut data: Vec<u8> = Vec::new();
        let mut at = 2;
        loop {
            let is_last = stream[at] == 1;
            let len = u16_at(stream, at + 1);
            assert_eq!(u16_at(stream, at + 3), !len);
            data.extend(&stream[at + 5..at + 5 + len as usize]);
            at += 5 + len as usize;
            if is_last {
                break;
            }
        }
        assert_eq!(at + 4, stream.len());
        assert_eq!(u32_be_at(stream, at), adler32(&data));
        data
    }

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn stored_streams_read_back_whatever_their_size() {
        let long: Vec<u8> = (0..MAX_STORED_BLOCK * 2 + 10).map(|i| (i % 251) as u8).collect();
        let cases: [&[u8]; 3] = [&[], &[1, 2, 3], &long];
        for data in cases {
            assert_eq!(inflate_stored(&zlib_stored(data)), data);
        }
    }

    #[test]
    fn png_has_a_valid_structure_and_holds_the_image() {
        let size = 200;
        let rgba = render(size);
        let png = encode_png(size, &rgba);
        assert_eq!(png[..8], PNG_SIGNATURE);

        let mut chunks = Vec::new();
        let mut at = 8;
        while at < png.len() {
            let len = u32_be_at(&png, at) as usize;
            assert_eq!(u32_be_at(&png, at + 8 + len), crc32(&png[at + 4..at + 8 + len]));
            chunks.push((&png[at + 4..at + 8], &png[at + 8..at + 8 + len]));
            at += 12 + len;
        }
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].0, b"IHDR");
        assert_eq!(chunks[1].0, b"IDAT");
        assert_eq!(chunks[2].0, b"IEND");
        assert!(chunks[2].1.is_empty());

        let header = chunks[0].1;
        assert_eq!((u32_be_at(header, 0), u32_be_at(header, 4)), (size, size));
        assert_eq!(header[8..], [8, 6, 0, 0, 0]);

        let filtered = inflate_stored(chunks[1].1);
        let rows: Vec<&[u8]> = filtered.chunks_exact(size as usize * 4 + 1).collect();
        assert_eq!(rows.len(), size as usize);
        for (row, expected) in rows.iter().zip(rgba.chunks_exact(size as usize * 4)) {
            assert_eq!(row[0], 0);
            assert_eq!(&row[1..], expected);
        }
    }

    #[test]
    fn icns_entries_are_big_endian_and_add_up_to_the_file() {
        let entries = [(*b"ic07", vec![1, 2, 3]), (*b"ic08", vec![4])];
        let icns = encode_icns(&entries);
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32_be_at(&icns, 4) as usize, icns.len());

        assert_eq!(&icns[8..12], b"ic07");
        assert_eq!(u32_be_at(&icns, 12), 8 + 3);
        assert_eq!(icns[16..19], [1, 2, 3]);
        assert_eq!(&icns[19..23], b"ic08");
        assert_eq!(u32_be_at(&icns, 23), 8 + 1);
        assert_eq!(icns[27], 4);
        assert_eq!(icns.len(), 28);
    }

    #[test]
    fn icns_covers_the_retina_sizes() {
        let sizes = ICNS_ENTRIES.map(|(_, size)| size);
        assert_eq!(sizes, [128, 256, 512, 1024]);
        assert_eq!(ICNS_ENTRIES[3].0, *b"ic10");
    }

    fn dibs() -> Vec<(u32, Vec<u8>)> {
        [32, 16]
            .map(|size| (size, encode_dib(size, &render(size))))
            .to_vec()
    }

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    #[test]
    fn the_design_has_a_building_on_a_tile_with_clear_corners() {
        assert_eq!(sample(0.0, 0.0, true), CLEAR);
        assert_eq!(sample(0.99, 0.99, true), CLEAR);
        // The chimney, the sky above the roof, and the ground below the building.
        assert_eq!(sample(0.25, 0.5, true), BUILDING);
        assert_eq!(sample(0.5, 0.3, true), BACKGROUND);
        assert_eq!(sample(0.5, 0.9, true), BACKGROUND);
    }

    #[test]
    fn the_roof_rises_to_the_right_within_each_tooth() {
        let width = (ROOF_RIGHT - ROOF_LEFT) / ROOF_TEETH;
        let start = ROOF_LEFT + width;
        let y = 0.5;
        assert_eq!(sample(start + width * 0.1, y, true), BACKGROUND);
        assert_eq!(sample(start + width * 0.9, y, true), BUILDING);
    }

    #[test]
    fn windows_are_only_cut_out_when_asked_for() {
        let x = ROOF_LEFT + (ROOF_RIGHT - ROOF_LEFT) / ROOF_TEETH / 2.0;
        let y = (WINDOW_TOP + WINDOW_BOTTOM) / 2.0;
        assert_eq!(sample(x, y, true), BACKGROUND);
        assert_eq!(sample(x, y, false), BUILDING);
    }

    #[test]
    fn render_antialiases_the_edge_and_keeps_its_colour() {
        for size in ICON_SIZES {
            let rgba = render(size);
            assert_eq!(rgba.len(), (size * size * 4) as usize);
            assert_eq!(rgba[3], 0, "the corner of the {size} image should be clear");
        }
        let rgba = render(64);
        let partial: Vec<&[u8]> = rgba
            .chunks_exact(4)
            .filter(|pixel| pixel[3] > 0 && pixel[3] < 255)
            .collect();
        assert!(!partial.is_empty());
        assert!(partial.iter().all(|pixel| pixel[..3] == BACKGROUND[..3]));
    }

    #[test]
    fn dib_is_bottom_up_bgra_with_a_mask_of_the_clear_pixels() {
        // Two rows of two pixels: red and clear on top, then opaque green and blue.
        let rgba = [255, 0, 0, 255, 0, 0, 0, 0, 0, 255, 0, 255, 0, 0, 255, 255];
        let dib = encode_dib(2, &rgba);
        assert_eq!(dib.len(), 40 + 16 + 8);
        assert_eq!(u32_at(&dib, 0), 40);
        assert_eq!((u32_at(&dib, 4), u32_at(&dib, 8)), (2, 4));
        assert_eq!(u16_at(&dib, 14), 32);
        assert_eq!(u32_at(&dib, 20), 16 + 8);
        assert_eq!(dib[40..48], [0, 255, 0, 255, 255, 0, 0, 255]);
        assert_eq!(dib[48..56], [0, 0, 255, 255, 0, 0, 0, 0]);
        assert_eq!(dib[56..60], [0, 0, 0, 0]);
        assert_eq!(dib[60..64], [0b0100_0000, 0, 0, 0]);
    }

    #[test]
    fn ico_entries_point_at_their_images() {
        let images = dibs();
        let ico = encode_ico(&images);
        assert_eq!(u16_at(&ico, 2), 1);
        assert_eq!(u16_at(&ico, 4), 2);
        for (index, (size, data)) in images.iter().enumerate() {
            let entry = 6 + 16 * index;
            assert_eq!(u32::from(ico[entry]), *size);
            assert_eq!(u32_at(&ico, entry + 8) as usize, data.len());
            let start = u32_at(&ico, entry + 12) as usize;
            assert_eq!(&ico[start..start + data.len()], data.as_slice());
        }
    }

    #[test]
    fn a_256_icon_is_recorded_as_size_zero() {
        assert_eq!(directory_entry(256, 0)[..2], [0, 0]);
    }

    #[test]
    fn res_holds_an_empty_record_then_the_icons_then_their_group() {
        let images = dibs();
        let res = encode_res(&images);

        let mut records = Vec::new();
        let mut at = 0;
        while at < res.len() {
            let data_len = u32_at(&res, at) as usize;
            assert_eq!(u32_at(&res, at + 4), 32);
            assert_eq!(u16_at(&res, at + 8), 0xFFFF);
            assert_eq!(u16_at(&res, at + 12), 0xFFFF);
            let data = &res[at + 32..at + 32 + data_len];
            records.push((u16_at(&res, at + 10), u16_at(&res, at + 14), data));
            at = (at + 32 + data_len).next_multiple_of(4);
        }
        assert_eq!(at, res.len());

        assert_eq!(records.len(), 4);
        assert_eq!((records[0].0, records[0].1, records[0].2.len()), (0, 0, 0));
        assert_eq!(records[1], (RT_ICON, 1, images[0].1.as_slice()));
        assert_eq!(records[2], (RT_ICON, 2, images[1].1.as_slice()));

        let (kind, id, group) = records[3];
        assert_eq!((kind, id), (RT_GROUP_ICON, GROUP_ID));
        assert_eq!(u16_at(group, 4), 2);
        assert_eq!(group.len(), 6 + 14 * 2);
        for (index, (size, data)) in images.iter().enumerate() {
            let entry = 6 + 14 * index;
            assert_eq!(u32::from(group[entry]), *size);
            assert_eq!(u32_at(group, entry + 8) as usize, data.len());
            assert_eq!(u16_at(group, entry + 12) as usize, index + 1);
        }
    }
}
