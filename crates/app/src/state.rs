//! App-wide reactive state, shared through Dioxus context.

use std::collections::HashSet;

use dioxus::prelude::*;
use margin_shared::{Id, Me};

use crate::api::{Api, ApiFailure};
use crate::config::{self, LocalConfig};
use crate::secrets;
use crate::sources::Upload;
use crate::store::LibraryState;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub api: Api,
    pub me: Option<Me>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoadStatus {
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveStatus {
    Connecting,
    Live,
    Reconnecting { error: String, retry_secs: u64 },
}

/// Everything the screens share. All fields are signals, so the struct is `Copy`.
#[derive(Clone, Copy)]
pub struct AppCtx {
    pub config: Signal<LocalConfig>,
    pub session: Signal<Option<Session>>,
    /// Set when the service signed this device out (revoked token), shown on the
    /// sign-in screen.
    pub signed_out_reason: Signal<Option<String>>,
    pub lib: Signal<LibraryState>,
    pub lib_status: Signal<LoadStatus>,
    /// Bumped to ask the live loop to reload the library now.
    pub reload: Signal<u64>,
    pub live: Signal<LiveStatus>,
    pub search_open: Signal<bool>,
    pub drawer_open: Signal<bool>,
    pub collapsed: Signal<HashSet<Id>>,
    /// The proposal shown on the Ask screen, kept while navigating elsewhere.
    pub ask_proposal: Signal<Option<Id>>,
    pub ask_prompt: Signal<String>,
    /// Files being added as sources, in the order added: the Ask composer's (kept, like
    /// the prompt, until its lessons are created) and those still uploading to a topic.
    pub uploads: Signal<Vec<Upload>>,
    /// The Settings dialog, open at this pane.
    pub settings_open: Signal<Option<crate::ui::settings::SettingsPane>>,
}

pub fn use_app() -> AppCtx {
    use_context::<AppCtx>()
}

impl AppCtx {
    pub fn api(&self) -> Option<Api> {
        self.session.peek().as_ref().map(|s| s.api.clone())
    }

    /// The service URL the app is configured for (or the default).
    pub fn service_url(&self) -> String {
        self.config
            .peek()
            .service_url
            .clone()
            .unwrap_or_else(|| config::DEFAULT_SERVICE_URL.to_string())
    }

    /// Handle a failed request: a revoked token signs this device out.
    /// Returns the message to show.
    pub fn failure(&self, failure: &ApiFailure) -> String {
        if failure.is_unauthorized() {
            self.forget_session(Some(
                "This device was signed out by the service. Sign in again.".into(),
            ));
        }
        failure.message.clone()
    }

    /// Drop the local token and session without calling the service.
    pub fn forget_session(&self, reason: Option<String>) {
        let mut this = *self;
        let url = self.service_url();
        let _ = secrets::delete_token(&url);
        this.signed_out_reason.set(reason);
        this.session.set(None);
        this.lib.set(LibraryState::default());
        this.lib_status.set(LoadStatus::Loading);
        this.ask_proposal.set(None);
        this.uploads.set(Vec::new());
    }

    /// Sign out: revoke this device on the service (best effort), then forget the token.
    pub async fn sign_out(&self) {
        let session = self.session.peek().clone();
        if let Some(session) = session {
            let device = match &session.me {
                Some(me) => Some(me.device.id.clone()),
                None => session.api.me().await.ok().map(|me| me.device.id),
            };
            if let Some(device) = device {
                let _ = session.api.revoke_device(&device).await;
            }
        }
        self.forget_session(None);
    }

    pub fn set_config(&self, update: impl FnOnce(&mut LocalConfig)) -> Result<(), String> {
        let mut this = *self;
        let mut next = self.config.peek().clone();
        update(&mut next);
        let result = config::save(&next);
        this.config.set(next);
        result
    }
}
