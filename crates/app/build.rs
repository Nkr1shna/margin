//! Android: put Margin's Kotlin (the source pickers, `android/`) into the Gradle project
//! dx generates. dx tells wry where its Kotlin files go with
//! `WRY_ANDROID_KOTLIN_FILES_OUT_DIR` and `WRY_ANDROID_PACKAGE`; Margin's files go to the
//! same place the same way (see wry's build.rs), so Gradle compiles them with the app.

use std::path::PathBuf;

const FILES: &[&str] = &["MarginPicker.kt", "proguard-margin.pro"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }
    println!("cargo:rerun-if-env-changed=WRY_ANDROID_PACKAGE");
    println!("cargo:rerun-if-env-changed=WRY_ANDROID_KOTLIN_FILES_OUT_DIR");
    let Ok(out_dir) = std::env::var("WRY_ANDROID_KOTLIN_FILES_OUT_DIR") else {
        // A plain `cargo build` for Android, outside dx: no Gradle project to write to.
        return;
    };
    let package = std::env::var("WRY_ANDROID_PACKAGE")
        .expect("dx sets WRY_ANDROID_PACKAGE along with WRY_ANDROID_KOTLIN_FILES_OUT_DIR");
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(out_dir);
    std::fs::create_dir_all(&out_dir).expect("couldn't create the Kotlin output folder");
    for name in FILES {
        let source = manifest_dir.join("android").join(name);
        println!("cargo:rerun-if-changed={}", source.display());
        // Kotlin escapes keywords in the package with backticks; ProGuard doesn't.
        let package = if name.ends_with(".pro") {
            package.replace('`', "")
        } else {
            package.clone()
        };
        let text = std::fs::read_to_string(&source)
            .unwrap_or_else(|e| panic!("couldn't read {}: {e}", source.display()))
            .replace("{{package}}", &package);
        let out = out_dir.join(name);
        // Write only when it changed, so Gradle doesn't rebuild for nothing.
        if std::fs::read_to_string(&out).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&out, text)
                .unwrap_or_else(|e| panic!("couldn't write {}: {e}", out.display()));
        }
        // dx can regenerate the Gradle project; a missing file reruns this script.
        println!("cargo:rerun-if-changed={}", out.display());
    }
}
