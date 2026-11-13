//! Encodes synthetic frames with the hardware encoder at desktop sizes clients
//! have asked for, starting from the bitrate the server picks, and recovering
//! the way the server does: after 5 empty encodes in a row, recreate the
//! encoder at 75% of the bitrate.
//! Needs Apple hardware: `cargo test -p macrdp-encode --test vt_sizes -- --ignored --nocapture`

use macrdp_encode::{create_encoder, screen_bitrate, EncoderPreference, Quality};

fn frame(w: u32, h: u32, i: u32) -> Vec<u8> {
    let mut f = vec![0u8; w as usize * 4 * h as usize];
    for (n, px) in f.chunks_exact_mut(4).enumerate() {
        let x = (n as u32 % w + i * 7) as u8;
        px.copy_from_slice(&[x, (n / w as usize) as u8, x ^ 0x55, 255]);
    }
    f
}

/// Frames encoded out of 40, and the bitrate it ended up at
fn encode_with_recovery(w: u32, h: u32) -> (usize, u32) {
    let mut bitrate = screen_bitrate(w, h, 60.0, Quality::HighQuality);
    let new = |b| create_encoder(w, h, 60.0, Quality::HighQuality, EncoderPreference::Hardware, false, b).unwrap();
    let mut enc = new(bitrate);
    let (mut ok, mut empty_in_a_row) = (0, 0);
    for i in 0..40 {
        match enc.encode_bgra(&frame(w, h, i), w, h, w as usize * 4) {
            Ok(e) if !e.data.is_empty() => {
                ok += 1;
                empty_in_a_row = 0;
            }
            _ => {
                empty_in_a_row += 1;
                if empty_in_a_row == 5 {
                    empty_in_a_row = 0;
                    bitrate = ((bitrate as f64 * 0.75) as u32).max(8_000_000);
                    enc = new(bitrate);
                }
            }
        }
    }
    (ok, bitrate)
}

#[test]
#[ignore]
fn every_client_size_produces_frames() {
    let sizes = [
        (1920, 1080), (1920, 1200), (1600, 1200), (2560, 1440), (1712, 1104),
        (1696, 1072), (3232, 1344), (3232, 1392), (3440, 1440), (2560, 1600), (1280, 800),
    ];
    let mut failing = vec![];
    for (w, h) in sizes {
        let start = screen_bitrate(w, h, 60.0, Quality::HighQuality);
        let (ok, end) = encode_with_recovery(w, h);
        println!("{w}x{h}: {ok}/40 frames, bitrate {:.0} -> {:.0} Mbps", start as f64 / 1e6, end as f64 / 1e6);
        if ok < 25 {
            failing.push((w, h));
        }
    }
    assert!(failing.is_empty(), "sizes that didn't recover: {failing:?}");
}
