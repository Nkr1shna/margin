//! The notice on the launch after a crash: the app quit unexpectedly last time, with the
//! report to copy (`diagnostics`).

use dioxus::prelude::*;

use crate::config::APP_NAME;
use crate::ui::common::{Icon, copy_to_clipboard};

/// How long "Copied" shows on the button.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_secs(2);

/// The last crash's notice, until it's dismissed. `report` is taken once at launch
/// (`diagnostics::take_last_crash`); `None` shows nothing.
#[component]
pub fn CrashNoticeHost(report: Option<String>) -> Element {
    let mut shown = use_signal(move || report);
    let Some(report) = shown.read().clone() else {
        return rsx! {};
    };
    rsx! {
        CrashNotice { report, on_dismiss: move |_| shown.set(None) }
    }
}

#[component]
pub fn CrashNotice(report: String, on_dismiss: EventHandler<()>) -> Element {
    let mut copied = use_signal(|| None::<Result<(), String>>);
    let mut copies = use_signal(|| 0u32);
    let place = crate::diagnostics::log_dir()
        .map(|d| tilde(&d.display().to_string()))
        .map(|d| format!(" A report was saved in {d}."))
        .unwrap_or_default();
    rsx! {
        div { class: "crash-notice", role: "status", "aria-live": "polite",
            div { class: "notice",
                Icon { name: "alert" }
                div { class: "body",
                    span { class: "notice-title", "{APP_NAME} quit unexpectedly last time" }
                    span { class: "faint",
                        "Sorry about that.{place} Copy the details to pass them on."
                    }
                    div { class: "row-actions",
                        button {
                            class: "btn btn-secondary btn-sm",
                            onclick: move |_| {
                                let report = report.clone();
                                async move {
                                    let n = *copies.peek() + 1;
                                    copies.set(n);
                                    copied.set(None);
                                    let result = copy_to_clipboard(report).await;
                                    let ok = result.is_ok();
                                    copied.set(Some(result));
                                    if ok {
                                        tokio::time::sleep(COPIED_FOR).await;
                                        if *copies.peek() == n {
                                            copied.set(None);
                                        }
                                    }
                                }
                            },
                            Icon { name: "copy" }
                            if matches!(*copied.read(), Some(Ok(()))) { "Copied" } else { "Copy details" }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            onclick: move |_| on_dismiss.call(()),
                            "Dismiss"
                        }
                    }
                    if let Some(Err(error)) = copied.read().clone() {
                        span { class: "save-state error", role: "alert", "{error}" }
                    }
                }
            }
        }
    }
}

/// `/Users/me/Library/Logs/Margin` as `~/Library/Logs/Margin`.
fn tilde(path: &str) -> String {
    let home = dirs::home_dir().map(|h| h.display().to_string());
    match home.as_deref().and_then(|h| path.strip_prefix(h)) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(non_snake_case)]
    fn WithCrash() -> Element {
        rsx! { CrashNoticeHost { report: Some("Panicked at x.rs:1:1".to_string()) } }
    }

    #[allow(non_snake_case)]
    fn WithoutCrash() -> Element {
        rsx! { CrashNoticeHost { report: None } }
    }

    fn render(app: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(app);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_notice_after_a_crash_offers_the_details_and_a_way_out() {
        let html = render(WithCrash);
        assert!(
            html.contains(&format!("{APP_NAME} quit unexpectedly last time")),
            "{html}"
        );
        assert!(html.contains("Copy details"));
        assert!(html.contains("Dismiss"));
        assert!(html.contains(r#"role="status""#));
        // The report itself is copied, not shown.
        assert!(!html.contains("x.rs"));
        if let Some(dir) = crate::diagnostics::log_dir() {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            assert!(html.contains(&name), "{html}");
        }
    }

    #[test]
    fn no_crash_no_notice() {
        assert!(!render(WithoutCrash).contains("crash-notice"));
    }

    #[test]
    fn home_is_shortened() {
        if let Some(home) = dirs::home_dir() {
            let p = home.join("Library/Logs/Margin");
            assert_eq!(tilde(&p.display().to_string()), "~/Library/Logs/Margin");
        }
        assert_eq!(tilde("/var/x"), "/var/x");
    }
}
