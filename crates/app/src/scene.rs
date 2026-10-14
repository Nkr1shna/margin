//! iOS and iPadOS: the UIKit scene lifecycle.
//!
//! Since iOS 27, UIKit stops an app at launch unless it adopts the scene lifecycle
//! (`UIApplicationEvaluateRuntimeIssueForNoSceneLifecycleAdoption`). tao 0.34, which
//! Dioxus 0.7 runs on, only knows the app-delegate lifecycle: it makes its `UIWindow`
//! with `initWithFrame:` while the app finishes launching, before any scene exists. tao
//! 0.37 adopts scenes, but Dioxus 0.7's mobile renderer is built on tao 0.34 and wry
//! 0.53, so it can't be swapped in underneath it.
//!
//! So Margin adopts the lifecycle itself. Info.plist (`[ios.plist]` in Dioxus.toml)
//! declares a `UIApplicationSceneManifest` whose one window-scene configuration names
//! `MarginSceneDelegate`, registered here before UIKit starts. A window only shows once
//! it belongs to a scene, so tao's window joins the scene when both exist: the scene
//! connects after launch finishes, and the app hands over its window from the root
//! component (after Dioxus made the window and its webview); whichever comes second
//! attaches them. The same path runs on iOS 26 and earlier, where the manifest also
//! switches UIKit to scenes.
//!
//! tao's app-delegate callbacks for becoming active and resigning (its `Resumed` and
//! `Suspended` events) don't fire under scenes; Dioxus doesn't use them. Going to the
//! background and back comes from the scene delegate instead (`state::set_foreground`).

use std::cell::RefCell;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::{ClassType, MainThreadOnly, Message, define_class};
use objc2_foundation::{NSObject, NSObjectProtocol};
use objc2_ui_kit::{
    UICoordinateSpace, UIResponder, UIScene, UISceneConnectionOptions, UISceneDelegate,
    UISceneSession, UIWindow, UIWindowScene, UIWindowSceneDelegate,
};

#[derive(Default)]
struct State {
    /// The connected window scene.
    scene: Option<Retained<UIWindowScene>>,
    /// The app's window, once the app handed it over.
    window: Option<Retained<UIWindow>>,
}

thread_local! {
    // UIKit and the root component both run on the main thread.
    static STATE: RefCell<State> = RefCell::default();
}

define_class!(
    // SAFETY: UIResponder has no subclassing requirements, and `SceneDelegate` doesn't
    // implement `Drop`.
    #[unsafe(super(UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MarginSceneDelegate"]
    struct SceneDelegate;

    unsafe impl NSObjectProtocol for SceneDelegate {}

    unsafe impl UISceneDelegate for SceneDelegate {
        #[unsafe(method(scene:willConnectToSession:options:))]
        fn will_connect(
            &self,
            scene: &UIScene,
            _session: &UISceneSession,
            _options: &UISceneConnectionOptions,
        ) {
            let Some(scene) = scene.downcast_ref::<UIWindowScene>() else {
                return;
            };
            let window = STATE.with_borrow_mut(|state| {
                state.scene = Some(scene.retain());
                state.window.clone()
            });
            if let Some(window) = window {
                attach(&window, scene);
            }
        }

        #[unsafe(method(sceneDidDisconnect:))]
        fn did_disconnect(&self, _scene: &UIScene) {
            STATE.with_borrow_mut(|state| state.scene = None);
        }

        // UIKit suspends the app a few seconds after it goes to the background; the live
        // loop closes the events socket first, so the service knows this device is gone
        // at once, and opens it again on the way back.
        #[unsafe(method(sceneDidEnterBackground:))]
        fn did_enter_background(&self, _scene: &UIScene) {
            crate::state::set_foreground(false);
        }

        #[unsafe(method(sceneWillEnterForeground:))]
        fn will_enter_foreground(&self, _scene: &UIScene) {
            crate::state::set_foreground(true);
        }
    }

    unsafe impl UIWindowSceneDelegate for SceneDelegate {}
);

/// Registers `MarginSceneDelegate` with the Objective-C runtime, so UIKit finds the class
/// Info.plist names. Call before the event loop starts (`UIApplicationMain`).
pub fn register() {
    let _ = SceneDelegate::class();
}

/// Hands the app's window (tao's `UIWindow`, from `WindowExtIOS::ui_window`) to the
/// scene, now or once it connects. Call on the main thread.
pub fn adopt_window(pointer: *mut c_void) {
    // SAFETY: tao hands out its live UIWindow; retaining it keeps it alive here.
    let Some(window) = (unsafe { Retained::retain(pointer.cast::<UIWindow>()) }) else {
        return;
    };
    let scene = STATE.with_borrow_mut(|state| {
        state.window = Some(window.clone());
        state.scene.clone()
    });
    if let Some(scene) = scene {
        attach(&window, &scene);
    }
}

fn attach(window: &UIWindow, scene: &UIWindowScene) {
    if window.windowScene().as_deref() == Some(scene) {
        return;
    }
    window.setWindowScene(Some(scene));
    // tao sized the window from the window builder's (desktop) size against the screen,
    // and the webview, made inside it, was then scaled about its centre: fill the scene.
    // From here on autoresizing keeps the root view and the webview filling the window,
    // and UIKit keeps the window filling the scene.
    // `coordinateSpace` is deprecated for `effectiveGeometry.coordinateSpace` (iOS 16);
    // it's the same space and exists on every iOS with scenes.
    #[allow(deprecated)]
    let bounds = scene.coordinateSpace().bounds();
    window.setFrame(bounds);
    if let Some(view) = window.rootViewController().and_then(|c| c.viewIfLoaded()) {
        view.setFrame(bounds);
        for subview in view.subviews().iter() {
            subview.setFrame(view.bounds());
        }
    }
    window.makeKeyAndVisible();
}
