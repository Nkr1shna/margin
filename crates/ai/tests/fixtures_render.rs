//! The scene rules in `write_scene`'s prompt produce scenes ManimGX runs: every fixture
//! in `tests/fixtures` (written by hand to those rules, one of them shown to the model
//! verbatim) goes through the real `manimgx inspect`, with a stand-in voice that returns
//! silence. Skipped, with a message, when `manimgx` isn't installed.

use std::path::{Path, PathBuf};
use std::process::Command;

fn manimgx() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("manimgx"))
                .collect()
        })
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(Path::new(&home).join(".local/bin/manimgx"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// A mono 16-bit 48 kHz WAV of `seconds` of silence.
fn silent_wav(seconds: u32) -> Vec<u8> {
    let rate: u32 = 48_000;
    let data_len = rate * seconds * 2;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.resize(44 + data_len as usize, 0);
    w
}

#[test]
fn fixtures_follow_the_prompt_rules_and_run_in_manimgx() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut scenes: Vec<PathBuf> = std::fs::read_dir(&fixtures)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
        .collect();
    scenes.sort();
    assert!(scenes.len() >= 2, "expected scene fixtures in {fixtures:?}");

    // The static rules, checked on every fixture whether or not ManimGX is here.
    for scene in &scenes {
        let code = std::fs::read_to_string(scene).unwrap();
        assert!(
            code.lines().any(|l| l == "import manimgx as m"),
            "{scene:?}"
        );
        assert_eq!(code.matches("class ").count(), 1, "one class: {scene:?}");
        assert!(code.contains("class LessonScene(m.Scene):"), "{scene:?}");
        assert!(code.contains("self.say("), "{scene:?}");
        assert!(!code.contains("voice"), "no voice: {scene:?}");
        // Rule 9: it opens on the title card, on screen from the first frame.
        let body = &code[code.find("def construct").unwrap()..];
        assert!(
            body.find("self.add(title)").is_some_and(|i| body.find("self.").unwrap() == i),
            "the first thing construct shows is the title, with self.add: {scene:?}"
        );
        // Rule 8: it ends on the poster shot, held, with nothing fading out after it.
        assert_eq!(
            code.lines().rev().find(|l| !l.trim().is_empty()),
            Some("        self.wait(2)"),
            "the last statement holds the poster shot: {scene:?}"
        );
        let last_play = code.rfind("self.play(").unwrap_or(0);
        assert!(
            last_play > code.rfind("self.say(").unwrap(),
            "the poster shot is played after the last line: {scene:?}"
        );
        let poster_shot = &code[last_play..];
        assert!(
            !poster_shot.contains("FadeOut") && !poster_shot.contains("Uncreate"),
            "nothing leaves after the poster shot: {scene:?}"
        );
        assert!(
            poster_shot.contains("FadeIn(poster_title)"),
            "the poster shot shows the title: {scene:?}"
        );
        for line in code
            .lines()
            .filter(|l| l.starts_with("import ") || l.starts_with("from "))
        {
            assert!(
                line == "import manimgx as m" || line == "import math",
                "only manimgx and math imports: {line} in {scene:?}"
            );
        }
    }

    let Some(manimgx) = manimgx() else {
        eprintln!("SKIPPED rendering the scene fixtures: manimgx isn't installed");
        return;
    };

    let work = std::env::temp_dir().join(format!("margin-ai-fixtures-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let wav = work.join("silence.wav");
    std::fs::write(&wav, silent_wav(4)).unwrap();

    for scene in &scenes {
        let name = scene.file_stem().unwrap().to_string_lossy().to_string();
        let dir = work.join(&name);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("scene.py");
        let code = std::fs::read_to_string(scene).unwrap();
        // How a renderer installs a voice on the contract's LessonScene: any function
        // from text to m.Speech, set on the class (as a staticmethod).
        let with_voice = format!(
            "{code}\n\nLessonScene.voice = staticmethod(lambda text: m.Speech({:?}, text=text))\n",
            wav.to_str().unwrap()
        );
        std::fs::write(&file, with_voice).unwrap();

        let out = Command::new(&manimgx)
            .arg("inspect")
            .arg(&file)
            .current_dir(&dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("no GPU adapter") || stdout.contains("no GPU adapter") {
            eprintln!("SKIPPED rendering {name}: no GPU adapter is visible here");
            continue;
        }
        assert!(
            out.status.success(),
            "manimgx inspect failed for {name} ({}):\n{stdout}\n{stderr}",
            out.status
        );
        assert!(stdout.contains("layout: no problems"), "{name}:\n{stdout}");
        eprintln!("{name}: {}", stdout.lines().next().unwrap_or(""));
    }
    let _ = std::fs::remove_dir_all(&work);
}
