//! App-wide reactive state, shared through Dioxus context.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::OnceLock;

use dioxus::prelude::*;
use margin_shared::{
    Citation, DeviceFrame, DeviceToolCall, DeviceToolResult, Id, Me, Person, TranscriptSegment,
};
use tokio::sync::{mpsc, oneshot, watch};

use crate::api::{Api, ApiFailure};
use crate::config::{self, LocalConfig};
use crate::folders::store::LocalFolders;
use crate::folders::sync::SyncState;
use crate::secrets;
use crate::sources::Upload;
use crate::store::LibraryState;
use crate::ui::outline::OutlineDraft;

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
    Reconnecting {
        error: String,
        retry_secs: u64,
    },
    /// iOS, iPadOS, Android: the app is in the background, and the socket stays closed
    /// until it comes back (`AppCtx::foreground`).
    Background,
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
    /// Outline cards' edits not yet created, by proposal, kept while navigating elsewhere.
    pub outline_drafts: Signal<HashMap<Id, OutlineDraft>>,
    /// The new chat's message, kept while navigating elsewhere.
    pub ask_prompt: Signal<String>,
    /// Files being added as sources, in the order added, while they upload to a topic.
    pub uploads: Signal<Vec<Upload>>,
    /// The Settings dialog, open at this pane.
    pub settings_open: Signal<Option<crate::ui::settings::SettingsPane>>,
    /// The source viewer, open on this lesson's citation.
    pub source_viewer: Signal<Option<SourceViewer>>,
    /// Audio and video sources' whole transcripts, as last fetched for the viewer.
    pub transcripts: Signal<HashMap<Id, Vec<TranscriptSegment>>>,
    /// The source viewer, open on a passage the agent showed (`show_passage`), which
    /// belongs to no lesson.
    pub shown_passage: Signal<Option<Citation>>,
    /// Sends a frame on the events socket. Set by the live loop once the service said
    /// hello on a connection, `None` while disconnected (a frame sent then is dropped;
    /// the service's deadline covers it).
    pub device_frames: Signal<Option<mpsc::UnboundedSender<DeviceFrame>>>,
    /// App-control calls from the agent, waiting for the shell to run them on the main
    /// thread (`ui::shell::Shell` drains it).
    pub agent_commands: Signal<VecDeque<AgentCommand>>,
    /// Whether the app is in the foreground. iOS, iPadOS and Android close the events
    /// socket while it isn't (the system suspends the app soon after); computers are
    /// always in the foreground here.
    pub foreground: Signal<bool>,
    /// The folders this device holds and whether it can still read them (copied from
    /// `folders::store`).
    pub folders: Signal<LocalFolders>,
    /// What each held folder's sync is doing (copied from the sync engine).
    pub folder_sync: Signal<BTreeMap<Id, SyncState>>,
    /// Folders attached to the new chat, sent with its first message.
    pub ask_workspaces: Signal<Vec<Id>>,
}

/// What the sign-in screen says after the service refused this device's token: the
/// service's own sentence when it gave a reason (a disabled person), else that the device
/// was signed out.
pub fn signed_out_reason(failure: &ApiFailure) -> String {
    if failure.status == Some(401) && failure.code != "unauthorized" && !failure.message.is_empty()
    {
        failure.message.clone()
    } else {
        "This device was signed out by the service. Sign in again.".into()
    }
}

/// An app-control tool call for the shell to run, and where its answer goes.
#[derive(Debug)]
pub struct AgentCommand {
    /// One of the app-control calls (`navigate`, `open_citation`, `show_passage`,
    /// `toggle_sidebar`).
    pub call: DeviceToolCall,
    /// The chat whose turn asked, for passages it found.
    pub chat_id: Id,
    pub reply: oneshot::Sender<DeviceToolResult>,
}

/// Where the platform's lifecycle (the iOS scene, Android's activity) reports going to
/// the background and back. It runs outside the Dioxus runtime, so it goes through this
/// channel; the root component copies it into `AppCtx::foreground`.
fn foreground_channel() -> &'static watch::Sender<bool> {
    static CHANNEL: OnceLock<watch::Sender<bool>> = OnceLock::new();
    CHANNEL.get_or_init(|| watch::Sender::new(true))
}

/// Report the app going to the background (`false`) or coming back (`true`). Any thread.
#[cfg_attr(not(any(target_os = "ios", target_os = "android")), allow(dead_code))]
pub fn set_foreground(foreground: bool) {
    foreground_channel().send_replace(foreground);
    update_active();
}

/// Follows `set_foreground`, starting from the current state.
pub fn foreground_changes() -> watch::Receiver<bool> {
    foreground_channel().subscribe()
}

/// Whether the system runs Margin's background task (`background::Work`): set by the
/// platform when the task starts running, cleared when it ends or expires.
fn continued_channel() -> &'static watch::Sender<bool> {
    static CHANNEL: OnceLock<watch::Sender<bool>> = OnceLock::new();
    CHANNEL.get_or_init(|| watch::Sender::new(false))
}

/// `foreground || continued`, kept by `set_foreground` and `set_continued`.
fn active_channel() -> &'static watch::Sender<bool> {
    static CHANNEL: OnceLock<watch::Sender<bool>> = OnceLock::new();
    CHANNEL.get_or_init(|| watch::Sender::new(true))
}

fn update_active() {
    let active = *foreground_channel().borrow() || *continued_channel().borrow();
    active_channel().send_if_modified(|now| std::mem::replace(now, active) != active);
}

/// Report the system running Margin's background task (`true`) or the task ending or
/// expiring (`false`). Any thread (the iOS task's handlers call it).
#[allow(dead_code)] // 5-ios calls it from the task's handlers.
pub fn set_continued(continued: bool) {
    continued_channel().send_replace(continued);
    update_active();
}

/// Whether the system runs Margin's background task now.
pub fn continued() -> bool {
    *continued_channel().borrow()
}

/// Whether the app is active: in the foreground, or kept running in the background by
/// its task (`docs/phase5-brief.md` D1). While active the events socket stays open and
/// the sync engine doesn't pause.
pub fn active() -> bool {
    *active_channel().borrow()
}

/// Follows `active`, starting from the current state.
#[allow(dead_code)]
pub fn active_changes() -> watch::Receiver<bool> {
    active_channel().subscribe()
}

/// Which citation the source viewer shows.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceViewer {
    pub lesson_id: Id,
    /// `Citation::n`.
    pub n: u32,
}

pub fn use_app() -> AppCtx {
    use_context::<AppCtx>()
}

impl AppCtx {
    pub fn api(&self) -> Option<Api> {
        self.session.peek().as_ref().map(|s| s.api.clone())
    }

    /// The service URL the app talks to (`LocalConfig::service_url`).
    pub fn service_url(&self) -> String {
        self.config.peek().service_url()
    }

    /// Handle a failed request: a revoked token signs this device out.
    /// Returns the message to show.
    pub fn failure(&self, failure: &ApiFailure) -> String {
        if failure.is_unauthorized() {
            self.forget_session(Some(signed_out_reason(failure)));
        }
        failure.message.clone()
    }

    /// The signed-in person (from `GET /api/me`), once known. Reading it subscribes.
    pub fn person(&self) -> Option<Person> {
        self.session
            .read()
            .as_ref()
            .and_then(|s| s.me.as_ref())
            .and_then(|m| m.person.clone())
    }

    /// Whether the signed-in person is an organisation admin (or the owner). Until `me`
    /// has loaded nobody is: a member never sees an admin's controls (or offers the
    /// service would refuse), and an admin's appear once their role is known.
    pub fn is_admin(&self) -> bool {
        self.person().is_some_and(|p| p.org_role.is_admin())
    }

    /// Change the signed-in person as the session holds them (their own row came back
    /// from a request, or an event changed it).
    pub fn update_person(&self, update: impl FnOnce(&mut Person)) {
        let mut session = self.session;
        if let Some(person) = session
            .write()
            .as_mut()
            .and_then(|s| s.me.as_mut())
            .and_then(|m| m.person.as_mut())
        {
            update(person);
        }
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
        this.outline_drafts.set(HashMap::new());
        this.uploads.set(Vec::new());
        this.source_viewer.set(None);
        this.shown_passage.set(None);
        this.transcripts.set(HashMap::new());
        this.agent_commands.set(VecDeque::new());
        this.device_frames.set(None);
        this.ask_workspaces.set(Vec::new());
        this.folder_sync.set(BTreeMap::new());
        // Handles stay on this device; their syncs stop until it signs in again.
        crate::folders::sync::engine().set_api(None);
    }

    /// Sign out: revoke this device on the service (best effort), then forget the token.
    pub async fn sign_out(&self) {
        let session = self.session.peek().clone();
        if let Some(session) = session {
            let device = match &session.me {
                Some(me) => me.device.as_ref().map(|d| d.id.clone()),
                None => session
                    .api
                    .me()
                    .await
                    .ok()
                    .and_then(|me| me.device)
                    .map(|d| d.id),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signed_out_reason_is_the_services_sentence_when_it_gave_one() {
        // The service still answers 401 `person_disabled` with the sentence in the moment
        // before a disabled person's devices are deleted (D10).
        let disabled = ApiFailure {
            status: Some(401),
            code: margin_shared::access_code::PERSON_DISABLED.into(),
            message: "The GitHub account @bob is disabled on this Margin. Ask an admin.".into(),
        };
        assert!(signed_out_reason(&disabled).contains("@bob is disabled"));
        // Once the device is gone it's a plain 401: signed out, no reason given.
        let gone = ApiFailure {
            status: Some(401),
            code: "unauthorized".into(),
            message: "Sign in again: this device isn't signed in.".into(),
        };
        assert_eq!(
            signed_out_reason(&gone),
            "This device was signed out by the service. Sign in again."
        );
    }
}
