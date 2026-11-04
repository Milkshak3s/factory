//! Embeds the icon in the exe, and leaves `factory.ico` beside it for the MSI build.

#[path = "src/icon.rs"]
mod icon;

use std::path::Path;
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");

    let images: Vec<(u32, Vec<u8>)> = icon::ICON_SIZES
        .iter()
        .map(|&size| (size, icon::encode_dib(size, &icon::render(size))))
        .collect();

    let out_dir = env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out_dir = Path::new(&out_dir);

    // OUT_DIR is <profile>/build/factory-<hash>/out, and <profile> is where the exe goes.
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR is inside the target directory");
    fs::write(profile_dir.join("factory.ico"), icon::encode_ico(&images))
        .expect("write factory.ico");

    // Only the MSVC linker takes a .res file directly. Elsewhere the exe goes without an icon,
    // though its window still gets one from main.rs.
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        let res_path = out_dir.join("factory.res");
        fs::write(&res_path, icon::encode_res(&images)).expect("write factory.res");
        println!("cargo:rustc-link-arg-bins={}", res_path.display());
    } else {
        println!("cargo:warning=not embedding the icon in the exe: this target isn't MSVC");
    }
}
