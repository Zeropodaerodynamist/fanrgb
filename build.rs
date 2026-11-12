// Embeds the application icon (the rendered `assets/design/fanrgb.ico`: 16/20 px hinted glyph,
// 24-256 px from the 3D render) together with the application manifest. Without that
// file the multi-size .ico is generated from the procedural icon as before.

include!("src/icon_art.rs");

use std::io::Write;
use std::path::PathBuf;

fn write_ico(path: &PathBuf) {
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 96, 128];
    let mut images: Vec<Vec<u8>> = Vec::new();
    for &size in &sizes {
        let rgba = render_icon_rgba(size);
        let mut img = Vec::new();
        let mask_row_bytes = size.div_ceil(32) * 4;
        let pixel_bytes = size * size * 4;
        let mask_bytes = mask_row_bytes * size;
        // BITMAPINFOHEADER (height is doubled: XOR image + AND mask).
        img.extend_from_slice(&40u32.to_le_bytes());
        img.extend_from_slice(&(size as i32).to_le_bytes());
        img.extend_from_slice(&((size * 2) as i32).to_le_bytes());
        img.extend_from_slice(&1u16.to_le_bytes());
        img.extend_from_slice(&32u16.to_le_bytes());
        img.extend_from_slice(&0u32.to_le_bytes());
        img.extend_from_slice(&(pixel_bytes + mask_bytes).to_le_bytes());
        img.extend_from_slice(&[0u8; 16]);
        // Bottom-up BGRA rows.
        for row in (0..size).rev() {
            for col in 0..size {
                let i = ((row * size + col) * 4) as usize;
                img.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
            }
        }
        img.extend(std::iter::repeat_n(0u8, mask_bytes as usize));
        images.push(img);
    }

    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (i, &size) in sizes.iter().enumerate() {
        let dim = if size >= 256 { 0u8 } else { size as u8 };
        ico.push(dim);
        ico.push(dim);
        ico.push(0);
        ico.push(0);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(images[i].len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += images[i].len() as u32;
    }
    for img in &images {
        ico.extend_from_slice(img);
    }
    std::fs::File::create(path).unwrap().write_all(&ico).unwrap();
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/design/fanrgb.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // Commit stamp for diagnostics reports (best effort; "unknown" without git). Re-run on commits
    // and staging; the dirty flag reflects the tree when this script last ran.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    let git = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty", "--abbrev=10"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=FANRGB_GIT={git}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // DLL planting defence for the executable's static imports (hid.dll, cfgmgr32.dll, pdh.dll, ...):
    // resolve them from System32 only, never from the (possibly user-writable) exe folder.
    // 0x800 = LOAD_LIBRARY_SEARCH_SYSTEM32. Run-time LoadLibrary calls are covered by
    // SetDefaultDllDirectories in main().
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let designed =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets").join("design").join("fanrgb.ico");
    let ico_path = if designed.is_file() {
        designed
    } else {
        let p = out_dir.join("app.ico");
        write_ico(&p);
        p
    };

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets").join("app.manifest");
    let esc = |p: &PathBuf| p.display().to_string().replace('\\', "\\\\");
    let rc = format!("1 ICON \"{}\"\n1 24 \"{}\"\n", esc(&ico_path), esc(&manifest));
    let rc_path = out_dir.join("app.rc");
    std::fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_required().unwrap();
}
