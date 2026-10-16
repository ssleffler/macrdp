//! Runs the real macrdp-server binary as the virtual display helper.
//! Briefly changes the Mac's screen, so it's ignored by default:
//! `cargo test -p macrdp-server --test virtual_display -- --ignored --test-threads=1`

use std::path::Path;

use core_graphics::display::CGDisplay;
use macrdp_capture::virtual_display::VirtualDisplay;

fn helper() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_macrdp-server"))
}

fn assert_display(vd: &VirtualDisplay, w: u64, h: u64) {
    let d = CGDisplay::new(vd.display_id());
    assert_eq!((d.pixels_wide(), d.pixels_high()), (w, h), "wrong size");
    assert!(vd.is_main(), "virtual display isn't main");
    for other in CGDisplay::active_displays().unwrap().into_iter().filter(|&o| o != vd.display_id()) {
        assert_eq!(CGDisplay::new(other).mirrors_display(), vd.display_id(), "display {other} not mirroring");
    }
}

#[test]
#[ignore]
fn back_to_back_sessions_get_their_own_size() {
    // Different sizes one after another, like reconnects with different windows
    for (w, h) in [(2560, 1440), (1600, 900), (3236, 1344), (1920, 1080)] {
        let vd = VirtualDisplay::spawn(helper(), w, h).expect("create");
        assert_display(&vd, w as u64, h as u64);
        let id = vd.display_id();
        drop(vd);
        assert!(!CGDisplay::active_displays().unwrap().contains(&id), "{w}x{h} display still there");
    }
    let main = CGDisplay::main();
    assert_eq!(main.mirrors_display(), 0, "Mac's display still mirroring");
}
