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

/// Every pointer is sent on a canvas of exactly this size: the classic cursor
/// size, and the largest RDP allows without the large-pointer extension
/// (which this server doesn't negotiate). Any other size gets resized by the
/// client (shrunk if bigger, padded if smaller) with the hotspot left as
/// sent, which draws the shape off its real position.
pub const MAX_CURSOR_SIDE: u16 = 32;

/// Largest canvas rendered before cropping. Mac cursor images carry a lot of
/// transparent padding (the arrow is a 10x17 shape on a 28x40 canvas).
const RENDER_SIDE: usize = 256;

/// Transparent margin kept around the visible shape
const MARGIN: usize = 1;

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

/// The rendered cursor canvas: premultiplied BGRA, top row first
struct Canvas {
    width: usize,
    height: usize,
    hot_x: i32,
    hot_y: i32,
    bgra: Vec<u8>,
}

fn snapshot(scale: f64) -> Option<Canvas> {
    let mut buf = vec![0u8; RENDER_SIDE * RENDER_SIDE * 4];
    let (mut w, mut h, mut hot_x, mut hot_y) = (0, 0, 0, 0);
    let ok = unsafe {
        macrdp_cursor_snapshot(
            scale.max(0.1),
            RENDER_SIDE as c_int,
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
    let (width, height) = (w as usize, h as usize);
    buf.truncate(width * height * 4);
    Some(Canvas { width, height, hot_x, hot_y, bgra: buf })
}

/// Renders the current system cursor at `scale` pixels per point: its visible
/// pixels, placed on a MAX_CURSOR_SIDE square canvas with the hotspot moved
/// along. A fully transparent cursor (macOS hides it while typing) becomes an
/// all-transparent pointer.
pub fn current_cursor(scale: f64) -> Option<CursorImage> {
    let mut scale = scale.max(0.1);
    let mut canvas = snapshot(scale)?;
    if let Some(bounds) = visible_bounds(&canvas) {
        // Visible part too big for RDP: render smaller, so image and hotspot
        // shrink together (the client would otherwise scale only the image)
        let longest = bounds.width.max(bounds.height);
        if longest > MAX_CURSOR_SIDE as usize {
            scale *= MAX_CURSOR_SIDE as f64 / longest as f64;
            canvas = snapshot(scale)?;
        }
    }
    Some(crop_to_visible(&canvas))
}

struct Bounds {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

/// Bounding box of the non-transparent pixels, with MARGIN around it
fn visible_bounds(canvas: &Canvas) -> Option<Bounds> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    for y in 0..canvas.height {
        for x in 0..canvas.width {
            if canvas.bgra[(y * canvas.width + x) * 4 + 3] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x0 == usize::MAX {
        return None;
    }
    let x = x0.saturating_sub(MARGIN);
    let y = y0.saturating_sub(MARGIN);
    Some(Bounds {
        x,
        y,
        width: (x1 + MARGIN + 1).min(canvas.width) - x,
        height: (y1 + MARGIN + 1).min(canvas.height) - y,
    })
}

/// Puts the canvas's visible pixels at the top-left of a MAX_CURSOR_SIDE
/// square (anything that doesn't fit is cut off), moves the hotspot along,
/// flips to bottom-up and undoes premultiplied alpha.
fn crop_to_visible(canvas: &Canvas) -> CursorImage {
    let side = MAX_CURSOR_SIDE as usize;
    let bounds = visible_bounds(canvas).unwrap_or(Bounds { x: 0, y: 0, width: 0, height: 0 });
    let (w, h) = (bounds.width.min(side), bounds.height.min(side));

    let mut data = vec![0u8; side * side * 4];
    for y in 0..h {
        let src = (bounds.y + y) * canvas.width + bounds.x;
        let src_row = &canvas.bgra[src * 4..(src + w) * 4];
        let dst = (side - 1 - y) * side; // bottom-up
        for (x, px) in src_row.chunks_exact(4).enumerate() {
            let a = px[3] as u32;
            let unpremultiply = |c: u8| if a == 0 { 0 } else { ((c as u32 * 255 + a / 2) / a).min(255) as u8 };
            data[(dst + x) * 4..(dst + x) * 4 + 4]
                .copy_from_slice(&[unpremultiply(px[0]), unpremultiply(px[1]), unpremultiply(px[2]), px[3]]);
        }
    }

    let clamp = |hot: i32, origin: usize| (hot - origin as i32).clamp(0, side as i32 - 1) as u16;
    CursorImage {
        width: MAX_CURSOR_SIDE,
        height: MAX_CURSOR_SIDE,
        hot_x: clamp(canvas.hot_x, bounds.x),
        hot_y: clamp(canvas.hot_y, bounds.y),
        bgra_bottom_up: data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canvas with one opaque white rectangle
    fn canvas(width: usize, height: usize, rect: (usize, usize, usize, usize), hot: (i32, i32)) -> Canvas {
        let mut bgra = vec![0u8; width * height * 4];
        let (rx, ry, rw, rh) = rect;
        for y in ry..ry + rh {
            for x in rx..rx + rw {
                bgra[(y * width + x) * 4..(y * width + x) * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
        Canvas { width, height, hot_x: hot.0, hot_y: hot.1, bgra }
    }

    #[test]
    fn crops_padding_and_keeps_hotspot_on_the_shape() {
        // An "I-beam": a 1-wide bar at x=14, y=10..30 on a 28x40 canvas, hotspot on the bar
        let c = canvas(28, 40, (14, 10, 1, 20), (14, 20));
        let img = crop_to_visible(&c);
        assert_eq!((img.width, img.height), (32, 32)); // always the full canvas
        assert_eq!((img.hot_x, img.hot_y), (1, 11)); // bar (plus 1px margin) at top-left, hotspot on it
        let px = |x: usize, y_from_top: usize| &img.bgra_bottom_up[((31 - y_from_top) * 32 + x) * 4..][..4];
        assert_eq!(px(1, 1)[3], 255, "bar pixel opaque");
        assert_eq!(px(1, 20)[3], 255, "bar bottom pixel opaque");
        assert_eq!(px(0, 1)[3], 0, "margin transparent");
        assert_eq!(px(1, 0)[3], 0, "top margin transparent");
        assert_eq!(px(1, 21)[3], 0, "below the bar transparent");
        assert_eq!(px(31, 31)[3], 0, "rest of the canvas transparent");
    }

    #[test]
    fn hotspot_outside_the_shape_is_clamped() {
        let c = canvas(20, 20, (5, 5, 4, 4), (0, 19));
        let img = crop_to_visible(&c);
        assert_eq!((img.hot_x, img.hot_y), (0, 15));
    }

    #[test]
    fn transparent_cursor_stays_fully_transparent() {
        let c = canvas(28, 40, (0, 0, 0, 0), (5, 5));
        let img = crop_to_visible(&c);
        assert_eq!((img.width, img.height), (32, 32));
        assert!(img.bgra_bottom_up.iter().all(|&b| b == 0));
    }

    #[test]
    fn unpremultiplies_alpha() {
        let mut c = canvas(3, 3, (1, 1, 1, 1), (1, 1));
        c.bgra[(1 * 3 + 1) * 4..(1 * 3 + 1) * 4 + 4].copy_from_slice(&[64, 32, 16, 128]); // premultiplied
        let img = crop_to_visible(&c);
        let center = &img.bgra_bottom_up[((31 - 1) * 32 + 1) * 4..][..4];
        assert_eq!(center, &[128, 64, 32, 128]);
    }

    /// Needs a logged-in GUI session: `cargo test -p macrdp-capture -- --ignored`
    #[test]
    #[ignore]
    fn renders_current_cursor() {
        let image = current_cursor(1.0).expect("no system cursor");
        assert_eq!((image.width, image.height), (MAX_CURSOR_SIDE, MAX_CURSOR_SIDE));
        assert!(image.hot_x < image.width && image.hot_y < image.height);
        let opaque = image.bgra_bottom_up.chunks_exact(4).filter(|p| p[3] > 0).count();
        println!("{}x{} hot=({},{}) opaque_px={opaque}", image.width, image.height, image.hot_x, image.hot_y);
        let big = current_cursor(4.0).unwrap();
        assert_eq!((big.width, big.height), (MAX_CURSOR_SIDE, MAX_CURSOR_SIDE));
        println!("scaled: {}x{} hot=({},{})", big.width, big.height, big.hot_x, big.hot_y);
    }
}
