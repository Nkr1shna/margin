//! macOS window chrome for the unified title bar: the traffic lights sit in the 46pt
//! top bar, vertically centered, instead of AppKit's default 28pt title bar.
//!
//! tao's `with_traffic_light_inset` only applies from its own view's `drawRect`, which
//! never runs under the webview, so the buttons are placed here directly and again
//! after every resize (AppKit lays the title bar out afresh then).

use dioxus::desktop::tao::event::{Event, WindowEvent};
use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
use dioxus::desktop::{use_wry_event_handler, window};
use objc2_app_kit::{NSWindow, NSWindowButton};

/// Height of the top bar, which `.topbar` and the sidebar's first row share (main.css).
const BAR_HEIGHT: f64 = 46.0;
/// Left edge of the close button.
const LEFT: f64 = 16.0;

/// Places the traffic lights now and after each resize. Call once, from the root.
pub fn use_traffic_lights() {
    dioxus::prelude::use_hook(place);
    use_wry_event_handler(|event, _| {
        if let Event::WindowEvent {
            event: WindowEvent::Resized(_) | WindowEvent::Focused(_),
            ..
        } = event
        {
            place();
        }
    });
}

fn place() {
    let ptr = window().window.ns_window() as *const NSWindow;
    // SAFETY: tao hands out its live NSWindow; this runs on the main thread.
    let Some(ns_window) = (unsafe { ptr.as_ref() }) else {
        return;
    };
    let buttons = [
        NSWindowButton::CloseButton,
        NSWindowButton::MiniaturizeButton,
        NSWindowButton::ZoomButton,
    ]
    .map(|kind| ns_window.standardWindowButton(kind));
    let [Some(close), Some(mini), Some(_)] = &buttons else {
        return;
    };
    // The buttons live in a title bar view inside a container that spans the window's
    // top; grow the container to the bar's height and center the buttons in it.
    // SAFETY: plain view-hierarchy reads on the main thread.
    let title_bar = unsafe { close.superview() };
    let Some(container) = title_bar.as_ref().and_then(|v| unsafe { v.superview() }) else {
        return;
    };
    let size = close.frame().size;
    let spacing = mini.frame().origin.x - close.frame().origin.x;
    let mut rect = container.frame();
    rect.size.height = BAR_HEIGHT;
    rect.origin.y = ns_window.frame().size.height - BAR_HEIGHT;
    container.setFrame(rect);
    if let Some(title_bar) = title_bar {
        let mut r = title_bar.frame();
        r.size.height = BAR_HEIGHT;
        r.origin.y = 0.0;
        title_bar.setFrame(r);
    }
    let y = ((BAR_HEIGHT - size.height) / 2.0).round();
    for (i, button) in buttons.iter().flatten().enumerate() {
        let mut origin = button.frame().origin;
        origin.x = LEFT + i as f64 * spacing;
        origin.y = y;
        button.setFrameOrigin(origin);
    }
}
