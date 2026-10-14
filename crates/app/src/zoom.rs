//! Page zoom: the reader's text size (View > Zoom In / Zoom Out / Actual Size on the
//! desktop, Settings > General > Text size everywhere), combined on iOS with the system
//! text size (Dynamic Type). The webview zooms natively where it can (desktop, iOS);
//! Android, which has no native page zoom, zooms with CSS.

use dioxus::prelude::*;

use crate::state::AppCtx;
use crate::ui::common::js;

/// The zoom levels offered, in percent.
pub const STEPS: &[u32] = &[75, 85, 100, 115, 130, 150, 175, 200];
pub const DEFAULT: u32 = 100;

/// The next larger level (the largest stays). View > Zoom In.
#[cfg(any(test, feature = "desktop"))]
pub fn zoom_in(current: u32) -> u32 {
    STEPS
        .iter()
        .copied()
        .find(|&s| s > current)
        .unwrap_or(*STEPS.last().unwrap_or(&DEFAULT))
}

/// The next smaller level (the smallest stays). View > Zoom Out.
#[cfg(any(test, feature = "desktop"))]
pub fn zoom_out(current: u32) -> u32 {
    STEPS
        .iter()
        .rev()
        .copied()
        .find(|&s| s < current)
        .unwrap_or(*STEPS.first().unwrap_or(&DEFAULT))
}

/// A saved level, kept within the offered range (hand-edited or older files).
pub fn clamp(level: u32) -> u32 {
    let (min, max) = (STEPS[0], STEPS[STEPS.len() - 1]);
    level.clamp(min, max)
}

/// The factor the webview zooms by: the reader's level times the system text size.
pub fn factor(level: u32, system_scale: f64) -> f64 {
    let system = if system_scale.is_finite() && system_scale > 0.0 {
        system_scale
    } else {
        1.0
    };
    (f64::from(clamp(level)) / 100.0 * system * 1000.0).round() / 1000.0
}

/// Applies the zoom whenever the level changes, and on iOS whenever the system text
/// size may have changed (the app comes back to the foreground). Call once, from the root.
pub fn use_page_zoom(ctx: AppCtx) {
    // iOS: follow Dynamic Type. Elsewhere the system scale stays 1.
    let mut system = use_signal(|| 1.0f64);
    use_future(move || async move {
        if !cfg!(target_os = "ios") {
            return;
        }
        let mut eval = js("dioxus.send(MarginApp.systemTextScale());\n\
             document.addEventListener('visibilitychange', function () {\n\
               if (!document.hidden) dioxus.send(MarginApp.systemTextScale());\n\
             });\n\
             await new Promise(function () {});");
        while let Ok(scale) = eval.recv::<f64>().await {
            if (scale - *system.peek()).abs() > 0.001 {
                system.set(scale);
            }
        }
    });
    use_effect(move || {
        let zoom = factor(ctx.config.read().zoom, *system.read());
        apply(zoom);
    });
}

fn apply(zoom: f64) {
    #[cfg(feature = "desktop")]
    dioxus::desktop::window().set_zoom_level(zoom);
    #[cfg(all(feature = "mobile", not(target_os = "android")))]
    dioxus::mobile::window().set_zoom_level(zoom);
    let css = cfg!(target_os = "android");
    let _ = js(&format!("MarginApp.setPageZoom({zoom}, {css});"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_up_and_down() {
        assert_eq!(zoom_in(100), 115);
        assert_eq!(zoom_out(100), 85);
        assert_eq!(zoom_in(200), 200);
        assert_eq!(zoom_out(75), 75);
        // Between steps (a hand-edited value) goes to the neighbouring step.
        assert_eq!(zoom_in(120), 130);
        assert_eq!(zoom_out(120), 115);
    }

    #[test]
    fn factor_combines_level_and_system_size() {
        assert_eq!(factor(100, 1.0), 1.0);
        assert_eq!(factor(150, 1.0), 1.5);
        // iOS "Larger" Dynamic Type: 23pt body against the 17pt default.
        assert_eq!(factor(100, 23.0 / 17.0), 1.353);
        // Out-of-range levels and bad system readings are tamed.
        assert_eq!(factor(10, 1.0), 0.75);
        assert_eq!(factor(100, f64::NAN), 1.0);
        assert_eq!(factor(100, 0.0), 1.0);
    }
}
