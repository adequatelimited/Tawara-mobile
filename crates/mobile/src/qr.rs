// Copyright (c) 2026 Adequate Systems HK Limited
// Licensed under the Mochimo Cryptocurrency Engine License Agreement 1.0; see LICENSE.md.

//! Platform-independent part of the Android QR scanner: frame size selection,
//! luminance extraction, decoding with rqrr, and the preview image.
//!
//! `android/scan.rs` supplies the camera frames. Because nothing here touches
//! the platform, the tests run on the host.

use std::panic::{AssertUnwindSafe, catch_unwind};

/// Maximum frame width requested by [`frame_size`], in pixels: large enough to
/// read a code at a normal distance, small enough to decode several times a
/// second.
pub(crate) const FRAME_WIDTH: i32 = 1280;
/// Maximum frame height requested by [`frame_size`], in pixels.
pub(crate) const FRAME_HEIGHT: i32 = 720;

/// Maximum length of the longer side of the [`preview`], in pixels. The preview
/// is uploaded on every drawn frame, so it is kept small.
pub(crate) const PREVIEW_LONGEST: usize = 320;

/// Chooses the frame size from the camera's `YUV_420_888` output sizes (width,
/// height in pixels): the largest within [`FRAME_WIDTH`] by [`FRAME_HEIGHT`],
/// otherwise the smallest. `None` when `sizes` is empty.
pub(crate) fn frame_size(sizes: &[(i32, i32)]) -> Option<(i32, i32)> {
    let fitting = sizes
        .iter()
        .filter(|&&(width, height)| width <= FRAME_WIDTH && height <= FRAME_HEIGHT);
    fitting
        .max_by_key(|&&(width, height)| i64::from(width) * i64::from(height))
        .or_else(|| {
            sizes
                .iter()
                .min_by_key(|&&(w, h)| i64::from(w) * i64::from(h))
        })
        .copied()
}

/// Copies the `width` by `height` luminance samples of a Y plane into a packed
/// buffer of `width * height` bytes, top row first. Rows are `row_stride` bytes
/// apart and samples `pixel_stride` bytes apart. `None` when a size is zero or
/// overflows, or the plane is too short for the layout.
pub(crate) fn luminance(
    plane: &[u8],
    width: usize,
    height: usize,
    row_stride: usize,
    pixel_stride: usize,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || pixel_stride == 0 {
        return None;
    }
    let last = (height - 1)
        .checked_mul(row_stride)?
        .checked_add((width - 1).checked_mul(pixel_stride)?)?;
    if last >= plane.len() {
        return None;
    }
    let mut out = Vec::with_capacity(width.checked_mul(height)?);
    for row in 0..height {
        // Every index below is at most `last`, which is within `plane`.
        let start = row * row_stride;
        if pixel_stride == 1 {
            out.extend_from_slice(&plane[start..start + width]);
        } else {
            out.extend((0..width).map(|x| plane[start + x * pixel_stride]));
        }
    }
    Some(out)
}

/// Decodes the first QR code in `gray`, a packed `width` by `height` luminance
/// frame as returned by [`luminance`], and returns its UTF-8 text as bytes.
/// `None` when no code decodes, `gray` does not match the size, or the decoder
/// panics: rqrr asserts on some malformed grids, and the panic is caught here so
/// that no frame can stop a scan.
pub(crate) fn read(width: usize, height: usize, gray: &[u8]) -> Option<Vec<u8>> {
    if !fits(width, height, gray.len()) {
        return None;
    }
    catch_unwind(AssertUnwindSafe(|| decode(width, height, gray)))
        .ok()
        .flatten()
}

/// Whether a non-empty `width` by `height` frame has exactly `len` samples.
fn fits(width: usize, height: usize, len: usize) -> bool {
    width > 0 && height > 0 && width.checked_mul(height) == Some(len)
}

fn decode(width: usize, height: usize, gray: &[u8]) -> Option<Vec<u8>> {
    let mut image =
        rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| gray[y * width + x]);
    let grids = image.detect_grids();
    let found = grids.iter().find_map(|grid| grid.decode().ok());
    found.map(|(_, text)| text.into_bytes())
}

/// Clockwise quarter turns that make a back-camera frame upright, given the
/// sensor orientation `sensor_degrees` (`ACAMERA_SENSOR_ORIENTATION`) and the
/// display rotation `display_degrees` (0, 90, 180 or 270). This is Android's
/// documented formula for the back camera.
pub(crate) fn turns(sensor_degrees: i32, display_degrees: i32) -> u32 {
    let degrees = (sensor_degrees - display_degrees).rem_euclid(360);
    (degrees / 90) as u32
}

/// Builds the preview of `gray`, a `width` by `height` luminance frame:
/// downsampled until its longer side is at most [`PREVIEW_LONGEST`], rotated
/// `turns` quarter turns clockwise, and converted to non-premultiplied RGBA, top
/// row first. Returns the width, height and pixels. `None` when the frame is
/// empty or does not match its size.
pub(crate) fn preview(
    width: usize,
    height: usize,
    gray: &[u8],
    turns: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    if !fits(width, height, gray.len()) {
        return None;
    }
    let step = width.max(height).div_ceil(PREVIEW_LONGEST);
    let (small_width, small_height) = ((width / step).max(1), (height / step).max(1));
    // Take the centre sample of each `step` by `step` block.
    let mut small = Vec::with_capacity(small_width * small_height);
    for y in 0..small_height {
        let source_y = (y * step + step / 2).min(height - 1);
        for x in 0..small_width {
            let source_x = (x * step + step / 2).min(width - 1);
            small.push(gray[source_y * width + source_x]);
        }
    }
    let (turned_width, turned_height, turned) =
        rotate(&small, small_width, small_height, turns % 4);
    let mut rgba = Vec::with_capacity(turned.len() * 4);
    for value in turned {
        rgba.extend_from_slice(&[value, value, value, 255]);
    }
    Some((
        u32::try_from(turned_width).ok()?,
        u32::try_from(turned_height).ok()?,
        rgba,
    ))
}

/// Rotates a `width` by `height` frame `turns` quarter turns clockwise and
/// returns the new width, height and pixels.
fn rotate(pixels: &[u8], width: usize, height: usize, turns: u32) -> (usize, usize, Vec<u8>) {
    let (new_width, new_height) = if turns.is_multiple_of(2) {
        (width, height)
    } else {
        (height, width)
    };
    let mut out = Vec::with_capacity(new_width * new_height);
    for y in 0..new_height {
        for x in 0..new_width {
            let (source_x, source_y) = match turns {
                0 => (x, y),
                1 => (y, height - 1 - x),
                2 => (width - 1 - x, height - 1 - y),
                _ => (width - 1 - y, x),
            };
            out.push(pixels[source_y * width + source_x]);
        }
    }
    (new_width, new_height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version 4, level Q code for the text on the first line (`text ...`),
    /// with `#` for a dark module. Generated with qrcode 0.13.0, the crate the
    /// Receive screen uses.
    const FIXTURE: &str = include_str!("../tests/fixtures/qr_destination.txt");

    /// The fixture's text and its modules, one row per line, `true` for dark.
    fn fixture() -> (&'static str, Vec<Vec<bool>>) {
        let mut lines = FIXTURE.lines();
        let text = lines
            .next()
            .and_then(|line| line.strip_prefix("text "))
            .expect("the fixture starts with its text");
        let modules = lines
            .map(|line| line.chars().map(|c| c == '#').collect())
            .collect();
        (text, modules)
    }

    /// Draws the modules as a luminance frame at `scale` pixels per module, dark
    /// modules black, with a four-module white quiet zone. Returns the width,
    /// height and samples.
    fn draw(modules: &[Vec<bool>], scale: usize) -> (usize, usize, Vec<u8>) {
        const QUIET: usize = 4;
        let side = (modules.len() + 2 * QUIET) * scale;
        let mut gray = vec![255; side * side];
        for (row, line) in modules.iter().enumerate() {
            for (column, dark) in line.iter().enumerate() {
                if !dark {
                    continue;
                }
                for y in 0..scale {
                    for x in 0..scale {
                        let py = (row + QUIET) * scale + y;
                        let px = (column + QUIET) * scale + x;
                        gray[py * side + px] = 0;
                    }
                }
            }
        }
        (side, side, gray)
    }

    /// Rotates `modules` `turns` quarter turns clockwise. Independent of
    /// `rotate`, so that a fault in one cannot mask the other.
    fn turned(modules: &[Vec<bool>], turns: usize) -> Vec<Vec<bool>> {
        let n = modules.len();
        (0..n)
            .map(|y| {
                (0..n)
                    .map(|x| match turns % 4 {
                        0 => modules[y][x],
                        1 => modules[n - 1 - x][y],
                        2 => modules[n - 1 - y][n - 1 - x],
                        _ => modules[x][n - 1 - y],
                    })
                    .collect()
            })
            .collect()
    }

    /// Nearest-neighbour resample of a `width` by `height` frame to `to_width`
    /// by `to_height`.
    fn resample(
        gray: &[u8],
        width: usize,
        height: usize,
        to_width: usize,
        to_height: usize,
    ) -> Vec<u8> {
        (0..to_height)
            .flat_map(|y| {
                (0..to_width)
                    .map(move |x| gray[(y * height / to_height) * width + x * width / to_width])
            })
            .collect()
    }

    /// Xorshift generator with a fixed seed, so noisy frames are reproducible.
    struct Noise(u64);

    impl Noise {
        fn word(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn the_code_is_read_upright_at_each_scale_tried() {
        let (text, modules) = fixture();
        for scale in [3, 6] {
            let (width, height, gray) = draw(&modules, scale);
            assert_eq!(
                read(width, height, &gray).as_deref(),
                Some(text.as_bytes()),
                "at {scale} pixels a module"
            );
        }
    }

    #[test]
    fn the_code_is_read_turned_by_each_quarter_turn() {
        let (text, modules) = fixture();
        for turns in 0..4 {
            let (width, height, gray) = draw(&turned(&modules, turns), 4);
            assert_eq!(
                read(width, height, &gray).as_deref(),
                Some(text.as_bytes()),
                "turned {turns} quarter turns"
            );
        }
    }

    #[test]
    fn the_code_is_read_at_sizes_between_whole_numbers_of_pixels() {
        let (text, modules) = fixture();
        let (width, height, gray) = draw(&modules, 4);
        for (to_width, to_height) in [(100, 100), (246, 246), (193, 193)] {
            let scaled = resample(&gray, width, height, to_width, to_height);
            assert_eq!(
                read(to_width, to_height, &scaled).as_deref(),
                Some(text.as_bytes()),
                "resampled to {to_width} by {to_height}"
            );
        }
    }

    #[test]
    fn the_code_is_read_inside_a_frame_the_camera_gives() {
        let (text, modules) = fixture();
        let (code_width, _, code) = draw(&modules, 6);
        let (width, height) = (1280, 720);
        let mut frame = vec![200; width * height];
        for y in 0..code_width {
            for x in 0..code_width {
                frame[(y + 200) * width + x + 700] = code[y * code_width + x];
            }
        }
        assert_eq!(
            read(width, height, &frame).as_deref(),
            Some(text.as_bytes())
        );
    }

    #[test]
    fn the_code_is_read_through_noise() {
        let (text, modules) = fixture();
        let (width, height, mut gray) = draw(&modules, 6);
        let mut noise = Noise(0x9E37_79B9_7F4A_7C15);
        for value in &mut gray {
            let word = noise.word();
            // Up to 24 levels either way, and about one sample in 300 set to
            // black or white. rqrr misses this code at six pixels per module
            // when one sample in 100 is flipped.
            let shift = (word % 49) as i32 - 24;
            *value = (i32::from(*value) + shift).clamp(0, 255) as u8;
            if (word >> 32).is_multiple_of(300) {
                *value = if (word >> 40) & 1 == 0 { 0 } else { 255 };
            }
        }
        assert_eq!(read(width, height, &gray).as_deref(), Some(text.as_bytes()));
    }

    #[test]
    fn a_frame_with_no_code_in_it_reads_nothing() {
        let (width, height) = (640, 360);
        let white = vec![255; width * height];
        let gradient: Vec<u8> = (0..width * height)
            .map(|i| ((i % width) * 255 / width) as u8)
            .collect();
        let checks: Vec<u8> = (0..width * height)
            .map(|i| {
                if (i % width / 8 + i / width / 8) % 2 == 0 {
                    0
                } else {
                    255
                }
            })
            .collect();
        let mut noise = Noise(7);
        let static_frame: Vec<u8> = (0..width * height).map(|_| noise.word() as u8).collect();
        for (case, frame) in [
            ("white", white),
            ("gradient", gradient),
            ("checks", checks),
            ("static", static_frame),
        ] {
            assert_eq!(read(width, height, &frame), None, "{case}");
        }
    }

    #[test]
    fn a_frame_that_does_not_match_its_size_reads_nothing() {
        assert_eq!(read(4, 4, &[0; 15]), None);
        assert_eq!(read(0, 4, &[]), None);
        assert_eq!(read(4, 0, &[]), None);
    }

    #[test]
    fn the_luminance_of_a_padded_plane_is_its_samples_row_by_row() {
        // Rows 10 bytes apart, samples 2 bytes apart; the bytes between are
        // padding and must not be read.
        let mut plane = vec![0xEE_u8; 16];
        for (i, value) in [1_u8, 2, 3].into_iter().enumerate() {
            plane[i * 2] = value;
        }
        for (i, value) in [4_u8, 5, 6].into_iter().enumerate() {
            plane[10 + i * 2] = value;
        }
        assert_eq!(luminance(&plane, 3, 2, 10, 2), Some(vec![1, 2, 3, 4, 5, 6]));
        let packed = [1_u8, 2, 3, 4, 5, 6];
        assert_eq!(luminance(&packed, 3, 2, 3, 1), Some(packed.to_vec()));
    }

    #[test]
    fn the_luminance_of_a_plane_too_short_for_its_layout_is_refused() {
        let plane = vec![0xEE_u8; 16];
        // The last sample is at index 14, so 14 bytes are too few.
        assert_eq!(luminance(&plane[..14], 3, 2, 10, 2), None);
        assert_eq!(luminance(&plane, 0, 2, 10, 2), None);
        assert_eq!(luminance(&plane, 3, 2, 10, 0), None);
    }

    #[test]
    fn a_frame_is_read_at_the_largest_size_that_fits_or_else_the_smallest() {
        let sizes = [(640, 480), (1280, 720), (1920, 1080), (320, 240)];
        assert_eq!(frame_size(&sizes), Some((1280, 720)));
        assert_eq!(
            frame_size(&[(1920, 1080), (3840, 2160)]),
            Some((1920, 1080))
        );
        let none: &[(i32, i32)] = &[];
        assert_eq!(frame_size(none), None);
    }

    #[test]
    fn the_preview_is_turned_as_asked_and_is_rgba() {
        // Three by two samples, rows 1 2 3 / 4 5 6.
        let gray = [1_u8, 2, 3, 4, 5, 6];
        let expected = [
            (0, vec![1, 2, 3, 4, 5, 6]),
            (1, vec![4, 1, 5, 2, 6, 3]),
            (2, vec![6, 5, 4, 3, 2, 1]),
            (3, vec![3, 6, 2, 5, 1, 4]),
        ];
        for (turns, grays) in expected {
            let (width, height, rgba) = preview(3, 2, &gray, turns).expect("a frame");
            let (want_width, want_height) = if turns.is_multiple_of(2) {
                (3, 2)
            } else {
                (2, 3)
            };
            assert_eq!((width, height), (want_width, want_height), "turns {turns}");
            let (pixels, _) = rgba.as_chunks::<4>();
            let got: Vec<u8> = pixels
                .iter()
                .map(|pixel| {
                    assert_eq!(pixel[0], pixel[1]);
                    assert_eq!(pixel[1], pixel[2]);
                    assert_eq!(pixel[3], 255);
                    pixel[0]
                })
                .collect();
            assert_eq!(got, grays, "turns {turns}");
        }
    }

    #[test]
    fn the_preview_is_no_longer_than_its_bound_on_its_longer_side() {
        let frame = vec![128; 1280 * 720];
        let (width, height, rgba) = preview(1280, 720, &frame, 0).expect("a frame");
        assert_eq!((width, height), (320, 180));
        assert_eq!(rgba.len(), 320 * 180 * 4);
        let portrait = vec![128; 720 * 1280];
        let (width, height, _) = preview(720, 1280, &portrait, 0).expect("a frame");
        assert_eq!((width, height), (180, 320));
    }

    #[test]
    fn a_preview_of_a_frame_that_does_not_match_its_size_is_refused() {
        assert!(preview(3, 2, &[0; 5], 0).is_none());
        assert!(preview(0, 2, &[], 0).is_none());
    }

    #[test]
    fn the_turns_bring_a_back_camera_upright_on_each_display_rotation() {
        assert_eq!(turns(90, 0), 1);
        assert_eq!(turns(90, 90), 0);
        assert_eq!(turns(90, 180), 3);
        assert_eq!(turns(90, 270), 2);
        assert_eq!(turns(270, 0), 3);
        assert_eq!(turns(0, 0), 0);
    }
}
