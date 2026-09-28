//! Bakes the Tamzen BDF fonts into the binary as 1-bit glyph tables, so the
//! program never touches a font file at runtime, and builds swayfin-thumbd (the KIO
//! thumbnail helper) with CMake.

use std::{env, fmt::Write as _, fs, path::Path, process::Command};

const CELL_W: u32 = 8;
const CELL_H: i32 = 16;
const DESCENT: i32 = 4;

const FONTS: &[(&str, &str)] = &[
    ("REGULAR", "assets/fonts/TamzenForPowerline8x16r.bdf"),
    ("BOLD", "assets/fonts/TamzenForPowerline8x16b.bdf"),
];

fn main() {
    let mut out = String::new();
    writeln!(out, "pub const CELL_W: usize = {CELL_W};").unwrap();
    writeln!(out, "pub const CELL_H: usize = {CELL_H};").unwrap();

    for (name, path) in FONTS {
        println!("cargo:rerun-if-changed={path}");
        let glyphs = parse_bdf(&fs::read_to_string(path).unwrap());

        writeln!(out, "pub static {name}: Face = Face {{ codepoints: &[").unwrap();
        for (cp, _) in &glyphs {
            write!(out, "{cp},").unwrap();
        }
        writeln!(out, "], bitmaps: &[").unwrap();
        for (_, rows) in &glyphs {
            writeln!(out, "{rows:?},").unwrap();
        }
        writeln!(out, "] }};").unwrap();
    }

    let out_dir = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&out_dir).join("font_gen.rs"), out).unwrap();

    build_thumbd(Path::new(&out_dir));
}

/// Builds the helper and tells the program where it is (SWAYFIN_THUMBD). Without CMake or
/// KDE Frameworks this only warns: swayfin then shows cached thumbnails only.
fn build_thumbd(out_dir: &Path) {
    println!("cargo:rerun-if-changed=thumbd/main.cpp");
    println!("cargo:rerun-if-changed=thumbd/CMakeLists.txt");
    let build = out_dir.join("thumbd");
    let run = |args: &[&str]| {
        Command::new("cmake")
            .args(args)
            .output()
            .map_err(|e| e.to_string())
            .and_then(|o| {
                if o.status.success() {
                    Ok(())
                } else {
                    Err(String::from_utf8_lossy(&o.stderr).into_owned())
                }
            })
    };
    let build_str = build.to_str().unwrap();
    let result = run(&[
        "-S",
        "thumbd",
        "-B",
        build_str,
        "-DCMAKE_BUILD_TYPE=Release",
    ])
    .and_then(|()| run(&["--build", build_str]));
    let path = match result {
        Ok(()) => build.join("swayfin-thumbd").to_string_lossy().into_owned(),
        Err(e) => {
            let first = e
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .to_string();
            println!("cargo:warning=swayfin-thumbd not built (only cached thumbnails): {first}");
            String::new()
        }
    };
    println!("cargo:rustc-env=SWAYFIN_THUMBD={path}");
}

/// Returns (codepoint, 16 rows of 8 bits, MSB = leftmost pixel), sorted by codepoint.
fn parse_bdf(src: &str) -> Vec<(u32, [u8; CELL_H as usize])> {
    let mut glyphs = Vec::new();
    let mut lines = src.lines();

    while let Some(line) = lines.next() {
        if !line.starts_with("STARTCHAR") {
            continue;
        }
        let (mut cp, mut bbx) = (None, (0u32, 0i32, 0i32, 0i32));
        for line in lines.by_ref() {
            let mut f = line.split_whitespace();
            match f.next() {
                Some("ENCODING") => cp = f.next().and_then(|v| v.parse::<i64>().ok()),
                Some("BBX") => {
                    let v: Vec<i32> = f.map(|n| n.parse().unwrap()).collect();
                    bbx = (v[0] as u32, v[1], v[2], v[3]);
                }
                Some("BITMAP") => break,
                _ => {}
            }
        }

        let (w, h, xoff, yoff) = bbx;
        let row_bytes = w.div_ceil(8) as usize;
        let top = CELL_H - DESCENT - (yoff + h);
        let mut rows = [0u8; CELL_H as usize];

        for (i, line) in lines.by_ref().take_while(|l| *l != "ENDCHAR").enumerate() {
            let y = top + i as i32;
            if !(0..CELL_H).contains(&y) {
                continue;
            }
            let bits = u32::from_str_radix(&line[..row_bytes * 2], 16).unwrap();
            // Left-align the glyph's w bits into a 32-bit word, then take the cell's 8.
            let aligned = bits << (32 - row_bytes as u32 * 8);
            let shifted = if xoff >= 0 {
                aligned >> xoff
            } else {
                aligned << -xoff
            };
            rows[y as usize] = (shifted >> (32 - CELL_W)) as u8;
        }

        if let Some(cp) = cp.filter(|&c| c >= 0) {
            glyphs.push((cp as u32, rows));
        }
    }

    glyphs.sort_by_key(|g| g.0);
    glyphs.dedup_by_key(|g| g.0);
    glyphs
}
