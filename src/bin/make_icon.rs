//! Generates assets/icon.png and assets/icon.ico.
//! Run with: cargo run --bin make_icon
//!
//! Drawn at 1024px with hard edges, then downscaled (gives clean anti-aliasing).

use image::{
    ExtendedColorType, Rgba, RgbaImage,
    codecs::ico::{IcoEncoder, IcoFrame},
    imageops::{FilterType, resize},
};
use std::{fs, fs::File, io::BufWriter};

const S: u32 = 1024;

fn in_rrect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let r = r.min((x1 - x0) / 2.0).min((y1 - y0) / 2.0);
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn lerp(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [0, 1, 2].map(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t) as u8)
}

fn hex(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

fn lighten(c: [u8; 3], amt: f32) -> [u8; 3] {
    lerp(c, [255, 255, 255], amt)
}

fn darken(c: [u8; 3], amt: f32) -> [u8; 3] {
    lerp(c, [0, 0, 0], amt)
}

fn put(img: &mut RgbaImage, x: u32, y: u32, c: [u8; 3]) {
    img.put_pixel(x, y, Rgba([c[0], c[1], c[2], 255]));
}

fn main() {
    let mut img = RgbaImage::from_pixel(S, S, Rgba([0, 0, 0, 0]));
    let s = S as f32;

    // device body
    let (bx0, by0, bx1, by1, br) = (24.0, 24.0, s - 24.0, s - 24.0, 210.0);
    // key well
    let (wx0, wy0, wx1, wy1, wr) = (110.0, 110.0, s - 110.0, s - 110.0, 120.0);

    let keys: [[u32; 3]; 3] = [
        [0xe74c3c, 0x3498db, 0x2ecc71], // cut / copy / paste
        [0x9b59b6, 0x5d6d7e, 0x1abc9c],
        [0xe67e22, 0xf1c40f, 0xff5e8a], // capture row
    ];
    let gap = 40.0;
    let pad = 52.0;
    let key = ((wx1 - wx0) - 2.0 * pad - 2.0 * gap) / 3.0;
    let kr = 58.0;

    for y in 0..S {
        for x in 0..S {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            if !in_rrect(fx, fy, bx0, by0, bx1, by1, br) {
                continue;
            }
            // body: vertical gradient + thin highlight rim at the top
            let t = (fy - by0) / (by1 - by0);
            let mut c = lerp(hex(0x454c5e), hex(0x15181f), t);
            if !in_rrect(fx, fy, bx0 + 10.0, by0 + 14.0, bx1 - 10.0, by1 - 6.0, br - 12.0) {
                c = lighten(c, if fy < s / 2.0 { 0.25 } else { 0.0 });
            }

            if in_rrect(fx, fy, wx0, wy0, wx1, wy1, wr) {
                c = lerp(hex(0x07080b), hex(0x111319), t);
                for (ry, row) in keys.iter().enumerate() {
                    for (rx, &kc) in row.iter().enumerate() {
                        let kx0 = wx0 + pad + rx as f32 * (key + gap);
                        let ky0 = wy0 + pad + ry as f32 * (key + gap);
                        let (kx1, ky1) = (kx0 + key, ky0 + key);
                        // soft glow around each key
                        if in_rrect(fx, fy, kx0 - 10.0, ky0 - 6.0, kx1 + 10.0, ky1 + 14.0, kr + 10.0) {
                            c = lerp(c, darken(hex(kc), 0.55), 0.6);
                        }
                        if in_rrect(fx, fy, kx0, ky0, kx1, ky1, kr) {
                            let kt = (fy - ky0) / key;
                            let base = hex(kc);
                            c = lerp(lighten(base, 0.28), darken(base, 0.18), kt);
                            // glossy highlight band on the upper part of the key
                            if in_rrect(fx, fy, kx0 + 16.0, ky0 + 12.0, kx1 - 16.0, ky0 + key * 0.42, kr - 18.0) {
                                c = lighten(c, 0.22);
                            }
                        }
                    }
                }
            }
            put(&mut img, x, y, c);
        }
    }

    fs::create_dir_all("assets").unwrap();
    resize(&img, 256, 256, FilterType::Lanczos3).save("assets/icon.png").unwrap();

    let sizes = [256u32, 64, 48, 32, 24, 16];
    let scaled: Vec<RgbaImage> = sizes.iter().map(|&n| resize(&img, n, n, FilterType::Lanczos3)).collect();
    let frames: Vec<IcoFrame> = scaled
        .iter()
        .map(|i| IcoFrame::as_png(i.as_raw(), i.width(), i.height(), ExtendedColorType::Rgba8).unwrap())
        .collect();
    let f = BufWriter::new(File::create("assets/icon.ico").unwrap());
    IcoEncoder::new(f).encode_images(&frames).unwrap();
    println!("wrote assets/icon.png and assets/icon.ico");
}
