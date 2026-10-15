//! Run the in-memory fake service for driving the app by hand:
//!
//! ```sh
//! cargo run -p margin-app --example fake_service            # http://127.0.0.1:8799
//! FAKE_BIND=127.0.0.1:9000 FAKE_MEDIA_DIR=/path/with/video.mp4+storyboard.png cargo run …
//! ```
//!
//! Sign-in is approved automatically on the second poll (the authorize URL it opens is
//! a placeholder GitHub URL). Outlines, lessons and videos are "generated" over a few
//! seconds with live events. This is test tooling, not the Margin service.
//!
//! Two people are there: Alice, the owner, and Bob, a member (`FAKE_AS=bob` signs in as
//! Bob), with the groups, agents and invitations Settings → Organisation shows.

#[path = "../src/fake.rs"]
mod fake;
#[path = "../src/org.rs"]
#[allow(dead_code)]
mod org;

#[tokio::main]
async fn main() {
    let bind = std::env::var("FAKE_BIND").unwrap_or_else(|_| "127.0.0.1:8799".into());
    let mut state = fake::FakeState::new();
    // FAKE_AS=bob signs in as the member; the owner (alice) otherwise.
    if std::env::var("FAKE_AS").as_deref() == Ok("bob") {
        state.login_as = fake::BOB.into();
    }
    state.simulate = true;
    state.auto_approve_after = Some(1);
    state.media_dir = std::env::var_os("FAKE_MEDIA_DIR").map(Into::into);
    let service = fake::FakeService::start_with(state, &bind).await;
    println!("fake Margin service on {}", service.url);
    tokio::signal::ctrl_c().await.ok();
}
