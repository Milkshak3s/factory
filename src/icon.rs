//! Factory's icon, drawn in code: a factory silhouette on an orange rounded square.
//!
//! The app renders it for its window, and `build.rs` compiles this same file to produce the icon
//! embedded in the exe and the one the MSI uses. Sizes are in pixels and images are top-down,
//! unpremultiplied RGBA.

/// The sizes in the `.ico` and in the exe, largest first.
pub const ICON_SIZES: [u32; 8] = [256, 64, 48, 40, 32, 24, 20, 16];

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

#[cfg(test)]
mod tests {
    use super::*;

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
