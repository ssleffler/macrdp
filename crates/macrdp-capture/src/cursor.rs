//! System cursor shape, for mirroring the Mac's cursor to the RDP client.

use std::os::raw::c_int;

extern "C" {
    fn macrdp_cursor_seed() -> c_int;
    fn macrdp_cursor_snapshot(
        scale: f64,
        max_side: c_int,
        out: *mut u8,
        out_cap: usize,
        out_w: *mut c_int,
        out_h: *mut c_int,
        out_hot_x: *mut c_int,
        out_hot_y: *mut c_int,
    ) -> c_int;
}

/// Largest pointer RDP clients accept without the large-pointer extension.
pub const MAX_CURSOR_SIDE: u16 = 96;

/// A cursor image ready for an RDP 32bpp pointer update.
#[derive(Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Straight-alpha BGRA, bottom row first (RDP pointer layout)
    pub bgra_bottom_up: Vec<u8>,
}

/// Changes whenever the system cursor changes; cheap to call often.
pub fn cursor_seed() -> i32 {
    unsafe { macrdp_cursor_seed() }
}

/// Renders the current system cursor at `scale` pixels per point.
pub fn current_cursor(scale: f64) -> Option<CursorImage> {
    let side = MAX_CURSOR_SIDE as usize;
    let mut buf = vec![0u8; side * side * 4];
    let (mut w, mut h, mut hot_x, mut hot_y) = (0, 0, 0, 0);
    let ok = unsafe {
        macrdp_cursor_snapshot(
            scale.max(0.1),
            MAX_CURSOR_SIDE as c_int,
            buf.as_mut_ptr(),
            buf.len(),
            &mut w,
            &mut h,
            &mut hot_x,
            &mut hot_y,
        )
    };
    if ok == 0 || w <= 0 || h <= 0 {
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    let row = w * 4;

    // Flip to bottom-up and undo premultiplied alpha.
    let mut data = Vec::with_capacity(row * h);
    for y in (0..h).rev() {
        for px in buf[y * row..(y + 1) * row].chunks_exact(4) {
            let a = px[3] as u32;
            let unpremultiply = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
            data.extend_from_slice(&[unpremultiply(px[0]), unpremultiply(px[1]), unpremultiply(px[2]), px[3]]);
        }
    }

    Some(CursorImage {
        width: w as u16,
        height: h as u16,
        hot_x: hot_x as u16,
        hot_y: hot_y as u16,
        bgra_bottom_up: data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a logged-in GUI session: `cargo test -p macrdp-capture -- --ignored`
    #[test]
    #[ignore]
    fn renders_current_cursor() {
        let image = current_cursor(1.0).expect("no system cursor");
        assert!(image.width > 0 && image.width <= MAX_CURSOR_SIDE);
        assert!(image.hot_x < image.width && image.hot_y < image.height);
        let opaque = image.bgra_bottom_up.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(opaque > 20, "cursor image is blank");
        println!("{}x{} hot=({},{}) opaque_px={opaque}", image.width, image.height, image.hot_x, image.hot_y);
        let big = current_cursor(4.0).unwrap();
        assert!(big.width <= MAX_CURSOR_SIDE && big.height <= MAX_CURSOR_SIDE);
        println!("scaled: {}x{}", big.width, big.height);
    }
}
