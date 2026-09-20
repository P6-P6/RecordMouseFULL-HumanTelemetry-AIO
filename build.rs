//! Embeds the application icon.
//!
//! Calls `rc.exe` from the Windows SDK directly rather than pulling in a
//! resource crate. Builds go through `build.ps1`, which sets up the MSVC
//! developer environment, so `rc.exe` is on PATH there. If it is not -- a bare
//! `cargo build` outside that environment -- the icon is skipped with a warning
//! rather than failing the build, because an icon is cosmetic and refusing to
//! compile over it would be obnoxious.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("app.res");
    let status = Command::new("rc.exe")
        .args(["/nologo", "/fo"])
        .arg(&out)
        .arg("app.rc")
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:rustc-link-arg={}", out.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe failed ({s}); building without an icon"),
        Err(e) => println!("cargo:warning=rc.exe not found ({e}); building without an icon"),
    }
}
