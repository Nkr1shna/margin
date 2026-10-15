//! iOS Simulator: embed the app's entitlements (`ios/Simulator.entitlements`) the way
//! Xcode does for simulator builds, as the `__TEXT,__entitlements` (XML) and
//! `__TEXT,__ents_der` (DER) sections of the executable. The simulator takes an app's
//! entitlements from there, not from its ad-hoc code signature, and without
//! `application-identifier` / `keychain-access-groups` the Data Protection keychain that
//! holds the device token refuses every call (errSecMissingEntitlement: "A required
//! entitlement isn't present"). Device builds get the real ones from the provisioning
//! profile when dx signs them (`dx build --ios --device …` / `--codesign`).
//!
//! Android: put Margin's Kotlin (the source pickers, `android/`) into the Gradle project
//! dx generates. dx tells wry where its Kotlin files go with
//! `WRY_ANDROID_KOTLIN_FILES_OUT_DIR` and `WRY_ANDROID_PACKAGE`; Margin's files go to the
//! same place the same way (see wry's build.rs), so Gradle compiles them with the app.

use std::path::PathBuf;

const FILES: &[&str] = &["MarginPicker.kt", "proguard-margin.pro"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    build_sha();
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("android") => android(),
        Ok("ios") if is_simulator() => simulator_entitlements(),
        _ => {}
    }
}

/// The commit this build is from, for crash reports (`MARGIN_BUILD_SHA`, src/diagnostics.rs):
/// apps.yml's `SHA`, else the Actions checkout's `GITHUB_SHA`, else `git rev-parse HEAD`,
/// else "unknown".
fn build_sha() {
    for var in ["SHA", "GITHUB_SHA"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let from_env = ["SHA", "GITHUB_SHA"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|s| !s.trim().is_empty()));
    let sha = match from_env {
        Some(sha) => sha,
        None => {
            // A local build: follow HEAD and the branch it points at, so a new commit
            // rebuilds with its sha.
            let branch = git(&["symbolic-ref", "-q", "HEAD"])
                .and_then(|r| git(&["rev-parse", "--git-path", &r]));
            for path in [git(&["rev-parse", "--git-path", "HEAD"]), branch]
                .into_iter()
                .flatten()
                // A missing path (a packed ref) would rerun this script on every build.
                .filter(|p| std::path::Path::new(p).exists())
            {
                println!("cargo:rerun-if-changed={path}");
            }
            git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into())
        }
    };
    let short: String = sha.trim().chars().take(12).collect();
    println!("cargo:rustc-env=MARGIN_BUILD_SHA={short}");
}

/// `aarch64-apple-ios-sim`, or `x86_64-apple-ios` (only ever a simulator).
fn is_simulator() -> bool {
    std::env::var("CARGO_CFG_TARGET_ABI").as_deref() == Ok("sim")
        || std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86_64")
}

fn simulator_entitlements() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let xml = manifest_dir.join("ios").join("Simulator.entitlements");
    println!("cargo:rerun-if-changed={}", xml.display());
    let der = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("Simulator.entitlements.der");
    // `derq` comes with macOS, where iOS builds run; Xcode encodes these with it too.
    let status = std::process::Command::new("/usr/bin/derq")
        .args(["query", "-f", "xml", "--raw", "-i"])
        .arg(&xml)
        .arg("-o")
        .arg(&der)
        .status()
        .expect("couldn't run /usr/bin/derq to encode the simulator entitlements");
    assert!(status.success(), "derq couldn't encode {}", xml.display());
    for (section, file) in [("__entitlements", &xml), ("__ents_der", &der)] {
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,{section},{}",
            file.display()
        );
    }
}

fn android() {
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
