use std::process::Command;

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    let res = format!("{out}/sevenmcp.res");
    // pictures for ui.rs, always rgba: the logo as it is, and sineware-chan cropped to her pixels
    image::open("res/logo.png").unwrap().to_rgba8().save(format!("{out}/logo.png")).unwrap();
    let chan = image::open("res/sineware-chan.png").unwrap().to_rgba8();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, _) in chan.enumerate_pixels().filter(|(_, _, p)| p[3] > 0) {
        (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
    }
    let chan = image::imageops::crop_imm(&chan, x0, y0, x1 - x0 + 1, y1 - y0 + 1).to_image();
    // tall enough for the column at 150% dpi
    let chan = image::imageops::resize(&chan, chan.width() * 1000 / chan.height(), 1000, image::imageops::FilterType::Lanczos3);
    chan.save(format!("{out}/sineware-chan.png")).unwrap();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    // ci gives a dev build its commit. it is a prerelease of the next patch, so it sorts after this release
    println!("cargo:rerun-if-env-changed=SEVENMCP_DEV");
    let shown = match std::env::var("SEVENMCP_DEV").ok().filter(|c| !c.is_empty()) {
        Some(commit) => {
            let patch: u32 = std::env::var("CARGO_PKG_VERSION_PATCH").unwrap().parse().unwrap();
            let (major, minor) = (std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(), std::env::var("CARGO_PKG_VERSION_MINOR").unwrap());
            format!("{major}.{minor}.{}-dev.{}", patch + 1, &commit[..commit.len().min(7)])
        }
        None => version.clone(),
    };
    println!("cargo:rustc-env=VERSION={shown}");
    let ok = Command::new("llvm-rc")
        .arg(format!("/DVERSION=\"{shown}\""))
        .arg(format!("/DVERSION_BIN={},0", version.replace('.', ",")))
        .args(["/FO", &res, "res/sevenmcp.rc"])
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "llvm-rc failed");
    println!("cargo:rustc-link-arg-bins={res}");
    let rustc = Command::new(std::env::var("RUSTC").unwrap()).arg("--version").output().unwrap();
    println!("cargo:rustc-env=RUSTC_VERSION={}", String::from_utf8_lossy(&rustc.stdout).trim());
    println!("cargo:rustc-env=TARGET={}", std::env::var("TARGET").unwrap());
    println!("cargo:rerun-if-changed=res");
}
