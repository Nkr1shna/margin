//! Measures how long a lesson video takes to render on this machine, in this OS's
//! sandbox: on a Linux guest that is ManimGX on Mesa lavapipe (CPU Vulkan) under
//! bubblewrap, the number `docs/plan-linux-service.md` §2.9 and risk 5 wait for; on the
//! Mac it is the Metal baseline under `sandbox-exec`.
//!
//! A measurement, not a check, so it is ignored by default. On the guest (manimgx
//! installed with `uv tool install --python 3.14 manimgx`, `bwrap` with user namespaces
//! allowed), from the workspace root:
//!
//! ```text
//! cargo test -p margin-video --release --test lavapipe -- --ignored --nocapture
//! ```
//!
//! `MARGIN_MEASURE_SIZES` picks the renders (default `1920x1080@30,1280x720@30`: the
//! service's default size and a smaller one), `MARGIN_MEASURE_SCENE` another scene file
//! (default `crates/ai/tests/fixtures/reference_scene.py`, the fixture that uses every
//! construct the scene prompt allows), `LP_NUM_THREADS` lavapipe's threads (passed into
//! the sandbox). The narration is silent (lines take their estimated length), so only
//! rendering is timed. Results are printed as a Markdown table and written to
//! `target/tmp/margin-video/lavapipe/measurement.json` with the machine's facts.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use margin_video::{
    RenderConfig, RenderJob, RenderProgress, SandboxMode, Voice, detect_tools, render,
};

struct Size {
    width: u32,
    height: u32,
    fps: u32,
}

fn sizes() -> Vec<Size> {
    let spec = std::env::var("MARGIN_MEASURE_SIZES")
        .unwrap_or_else(|_| "1920x1080@30,1280x720@30".to_string());
    spec.split(',')
        .map(|s| {
            let (dims, fps) = s.trim().split_once('@').expect("WxH@FPS");
            let (w, h) = dims.split_once('x').expect("WxH@FPS");
            Size {
                width: w.parse().expect("width"),
                height: h.parse().expect("height"),
                fps: fps.parse().expect("fps"),
            }
        })
        .collect()
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The machine's facts for the report.
fn machine() -> serde_json::Value {
    let os = read("/etc/os-release")
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                .map(|v| v.trim_matches('"').to_string())
        })
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    let cpu = read("/proc/cpuinfo")
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("model name") || l.starts_with("CPU part"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        })
        .or_else(|| {
            std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "machdep.cpu.brand_string"])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        });
    let memory_kb = read("/proc/meminfo").and_then(|t| {
        t.lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
    });
    let bwrap = margin_video::bubblewrap::find_bwrap().and_then(|b| {
        let out = std::process::Command::new(b)
            .arg("--version")
            .output()
            .ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    serde_json::json!({
        "os": os,
        "arch": std::env::consts::ARCH,
        "cpu": cpu,
        "cpus": std::thread::available_parallelism().map(|n| n.get()).ok(),
        "memory_mb": memory_kb.map(|k| k / 1024),
        "bwrap": bwrap,
        "lp_num_threads": std::env::var("LP_NUM_THREADS").ok(),
        "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
    })
}

#[tokio::test]
#[ignore = "a measurement; run by hand on the Linux guest (see the file's header)"]
async fn lavapipe_render_time() {
    let tools = detect_tools().await;
    let manimgx = tools
        .iter()
        .find(|t| t.name == "manimgx" && t.found)
        .expect("manimgx must be installed (uv tool install --python 3.14 manimgx)");
    let scene_path = std::env::var("MARGIN_MEASURE_SCENE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../ai/tests/fixtures/reference_scene.py")
        });
    let scene = std::fs::read_to_string(&scene_path)
        .unwrap_or_else(|e| panic!("can't read {}: {e}", scene_path.display()));
    let sandbox = SandboxMode::native();
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("margin-video")
        .join("lavapipe");
    std::fs::create_dir_all(&root).unwrap();

    let mut rows = Vec::new();
    for size in sizes() {
        let label = format!("{}x{}@{}", size.width, size.height, size.fps);
        let work_dir = root.join(&label);
        std::fs::remove_dir_all(&work_dir).ok();
        let config = RenderConfig {
            manimgx: PathBuf::from(manimgx.path.clone().unwrap()),
            voice: Voice::Silent,
            sandbox,
            width: size.width,
            height: size.height,
            fps: size.fps,
        };
        let job = RenderJob {
            work_dir,
            scene_code: scene.clone(),
        };
        let start = Instant::now();
        let mut pass2_start: Option<Instant> = None;
        let mut log = Vec::new();
        let result = render(&job, &config, |p| {
            if let RenderProgress::Log(line) = &p {
                if line.starts_with("margin: pass 2") {
                    pass2_start = Some(Instant::now());
                }
                log.push(line.clone());
            }
        })
        .await;
        let total = start.elapsed();
        let out = result.unwrap_or_else(|e| {
            let tail = log[log.len().saturating_sub(30)..].join("\n");
            panic!("{label}: {e}\n{tail}")
        });
        let pass2 = pass2_start.map_or(total, |t| t.elapsed());
        let pass1 = total.saturating_sub(pass2);
        let video = out.duration_seconds.unwrap_or(0.0);
        let secs = |d: Duration| (d.as_secs_f64() * 10.0).round() / 10.0;
        rows.push(serde_json::json!({
            "size": label,
            "video_seconds": (f64::from(video) * 10.0).round() / 10.0,
            "pass1_seconds": secs(pass1),
            "pass2_seconds": secs(pass2),
            "total_seconds": secs(total),
            "render_seconds_per_video_second": if video > 0.0 {
                ((pass2.as_secs_f64() / f64::from(video)) * 100.0).round() / 100.0
            } else {
                0.0
            },
            "mp4_bytes": std::fs::metadata(&out.mp4).map(|m| m.len()).unwrap_or(0),
        }));
    }

    let report = serde_json::json!({
        "scene": scene_path.file_name().map(|n| n.to_string_lossy().into_owned()),
        "sandbox": format!("{sandbox:?}"),
        "manimgx": manimgx.version,
        "machine": machine(),
        "renders": rows,
    });
    let path = root.join("measurement.json");
    std::fs::write(&path, serde_json::to_string_pretty(&report).unwrap()).unwrap();

    println!(
        "\n{}",
        serde_json::to_string_pretty(&report["machine"]).unwrap()
    );
    println!("\n| size | video s | pass 1 s | pass 2 s | total s | render s per video s |");
    println!("|---|---|---|---|---|---|");
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            r["size"].as_str().unwrap_or(""),
            r["video_seconds"],
            r["pass1_seconds"],
            r["pass2_seconds"],
            r["total_seconds"],
            r["render_seconds_per_video_second"]
        );
    }
    println!("\nwritten to {}", path.display());
}
