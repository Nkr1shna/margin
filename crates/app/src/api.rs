//! HTTP client for the Margin service, coded against `margin_shared::routes`.
//! Every failure comes back as an [`ApiFailure`] whose `message` is fit to show in the UI.

use std::fmt;
use std::time::Duration;

use margin_shared::{
    AcceptProposal, ApiError, AskRequest, Device, EmbeddingModel, Id, Lesson, LessonPatch, Library,
    LoginState, Me, NewNote, NewTopic, Note, NotePatch, PollLoginRequest, Proposal, SearchHit,
    Settings, SettingsPatch, Source, StartLogin, StartLoginRequest, ToolStatus, Topic, TopicPatch,
    Video, routes,
};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone, PartialEq)]
pub struct ApiFailure {
    /// HTTP status, `None` for network or decoding failures.
    pub status: Option<u16>,
    /// `ApiError.code` from the service, or "network" / "decode" / "http".
    pub code: String,
    pub message: String,
}

impl ApiFailure {
    pub fn is_unauthorized(&self) -> bool {
        self.status == Some(401) || self.code == "unauthorized"
    }

    pub fn is_no_api_key(&self) -> bool {
        self.code == "no_api_key"
    }

    /// The Settings pane that fixes this error, if one does.
    pub fn settings_pane(&self) -> Option<crate::ui::settings::SettingsPane> {
        use crate::ui::settings::SettingsPane;
        match self.code.as_str() {
            "no_api_key" => Some(SettingsPane::Ai),
            "no_embedding_model" => Some(SettingsPane::Sources),
            c if c.contains("tool") => Some(SettingsPane::Tools),
            _ => None,
        }
    }

    pub fn local(message: impl Into<String>) -> Self {
        Self {
            status: None,
            code: "local".into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

pub type ApiResult<T> = Result<T, ApiFailure>;

#[derive(Clone)]
pub struct Api {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl PartialEq for Api {
    fn eq(&self, other: &Self) -> bool {
        self.base == other.base && self.token == other.token
    }
}

impl fmt::Debug for Api {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Api")
            .field("base", &self.base)
            .field("signed_in", &self.token.is_some())
            .finish()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

impl Api {
    /// A client without a token, for `/health` and `/auth/*`.
    pub fn anonymous(base: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: None,
            http: client(),
        }
    }

    pub fn with_token(base: &str, token: &str) -> Self {
        Self {
            token: Some(token.to_string()),
            ..Self::anonymous(base)
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let builder = self.http.request(method, self.url(path));
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn send(&self, builder: RequestBuilder) -> ApiResult<Response> {
        let response = builder.send().await.map_err(|e| self.network_failure(e))?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(failure_from(response).await)
        }
    }

    fn network_failure(&self, err: reqwest::Error) -> ApiFailure {
        let what = if err.is_timeout() {
            "timed out".to_string()
        } else if err.is_connect() {
            "couldn't connect".to_string()
        } else {
            root_cause(&err)
        };
        ApiFailure {
            status: None,
            code: "network".into(),
            message: format!("Couldn't reach the service at {} ({what}).", self.base),
        }
    }

    async fn json<T: DeserializeOwned>(&self, builder: RequestBuilder) -> ApiResult<T> {
        let response = self.send(builder).await?;
        response.json::<T>().await.map_err(|e| ApiFailure {
            status: None,
            code: "decode".into(),
            message: format!(
                "The service sent a response this app doesn't understand ({}).",
                root_cause(&e)
            ),
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.json(self.request(Method::GET, path)).await
    }

    async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.json(self.request(Method::POST, path).json(body)).await
    }

    async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        self.json(self.request(Method::POST, path)).await
    }

    async fn patch<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.json(self.request(Method::PATCH, path).json(body))
            .await
    }

    async fn delete(&self, path: &str) -> ApiResult<()> {
        self.send(self.request(Method::DELETE, path))
            .await
            .map(|_| ())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    pub async fn health(&self) -> ApiResult<()> {
        self.send(self.request(Method::GET, routes::HEALTH))
            .await
            .map(|_| ())
    }

    pub async fn start_login(&self, device_name: &str) -> ApiResult<StartLogin> {
        let body = StartLoginRequest {
            device_name: device_name.to_string(),
        };
        self.post(routes::AUTH_START, &body).await
    }

    pub async fn poll_login(&self, login: &StartLogin) -> ApiResult<LoginState> {
        let body = PollLoginRequest {
            login_id: login.login_id.clone(),
            poll_secret: login.poll_secret.clone(),
        };
        self.post(routes::AUTH_POLL, &body).await
    }

    pub async fn me(&self) -> ApiResult<Me> {
        self.get(routes::ME).await
    }

    pub async fn library(&self) -> ApiResult<Library> {
        self.get(routes::LIBRARY).await
    }

    pub async fn create_topic(&self, body: &NewTopic) -> ApiResult<Topic> {
        self.post(routes::TOPICS, body).await
    }

    pub async fn patch_topic(&self, id: &str, body: &TopicPatch) -> ApiResult<Topic> {
        self.patch(&routes::topic(id), body).await
    }

    pub async fn delete_topic(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::topic(id)).await
    }

    pub async fn patch_lesson(&self, id: &str, body: &LessonPatch) -> ApiResult<Lesson> {
        self.patch(&routes::lesson(id), body).await
    }

    pub async fn delete_lesson(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::lesson(id)).await
    }

    pub async fn regenerate_lesson(&self, id: &str) -> ApiResult<Lesson> {
        self.post_empty(&routes::lesson_regenerate(id)).await
    }

    pub async fn create_note(&self, body: &NewNote) -> ApiResult<Note> {
        self.post(routes::NOTES, body).await
    }

    pub async fn patch_note(&self, id: &str, body: &NotePatch) -> ApiResult<Note> {
        self.patch(&routes::note(id), body).await
    }

    pub async fn delete_note(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::note(id)).await
    }

    pub async fn ask(&self, body: &AskRequest) -> ApiResult<Proposal> {
        self.post(routes::ASK, body).await
    }

    pub async fn proposal(&self, id: &str) -> ApiResult<Proposal> {
        self.get(&routes::proposal(id)).await
    }

    pub async fn accept_proposal(&self, id: &str, body: &AcceptProposal) -> ApiResult<Topic> {
        self.post(&routes::proposal_accept(id), body).await
    }

    pub async fn make_video(&self, lesson_id: &str) -> ApiResult<Video> {
        self.post_empty(&routes::lesson_video(lesson_id)).await
    }

    pub async fn video(&self, id: &str) -> ApiResult<Video> {
        self.get(&routes::video(id)).await
    }

    pub async fn search(&self, query: &str) -> ApiResult<Vec<SearchHit>> {
        self.json(
            self.request(Method::GET, routes::SEARCH)
                .query(&[("q", query)]),
        )
        .await
    }

    pub async fn settings(&self) -> ApiResult<Settings> {
        self.get(routes::SETTINGS).await
    }

    pub async fn patch_settings(&self, body: &SettingsPatch) -> ApiResult<Settings> {
        self.patch(routes::SETTINGS, body).await
    }

    pub async fn tools(&self) -> ApiResult<Vec<ToolStatus>> {
        self.get(routes::TOOLS).await
    }

    pub async fn devices(&self) -> ApiResult<Vec<Device>> {
        self.get(routes::DEVICES).await
    }

    pub async fn revoke_device(&self, id: &Id) -> ApiResult<()> {
        self.delete(&routes::device(id)).await
    }

    /// Upload one file as a source. `name` is the file name, or its path inside the folder
    /// it was added with; without `topic_id` it waits to be named in an `AskRequest`.
    pub async fn upload_source(
        &self,
        bytes: Vec<u8>,
        name: &str,
        folder: Option<&str>,
        topic_id: Option<&str>,
    ) -> ApiResult<Source> {
        if bytes.len() as u64 > routes::MAX_SOURCE_BYTES {
            return Err(ApiFailure::local(format!(
                "{name} is larger than {} MB, the most a source can be.",
                routes::MAX_SOURCE_BYTES / (1024 * 1024)
            )));
        }
        let mut query = vec![("name", name)];
        if let Some(folder) = folder {
            query.push(("folder", folder));
        }
        if let Some(topic_id) = topic_id {
            query.push(("topic_id", topic_id));
        }
        let builder = self
            .request(Method::POST, routes::SOURCES)
            .query(&query)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            // Large files over a slow link take longer than the default 60 s.
            .timeout(Duration::from_secs(600))
            .body(bytes);
        self.json(builder).await
    }

    pub async fn delete_source(&self, id: &str) -> ApiResult<()> {
        self.delete(&routes::source(id)).await
    }

    pub async fn embedding(&self) -> ApiResult<EmbeddingModel> {
        self.get(routes::EMBEDDING).await
    }

    /// Start downloading the embedding model on the service Mac; progress arrives as
    /// `EmbeddingModelChanged` events.
    pub async fn download_embedding(&self) -> ApiResult<EmbeddingModel> {
        self.post_empty(routes::EMBEDDING_DOWNLOAD).await
    }

    /// Delete the downloaded model files.
    pub async fn delete_embedding(&self) -> ApiResult<EmbeddingModel> {
        self.json(self.request(Method::DELETE, routes::EMBEDDING))
            .await
    }

    /// Fetch a text file from an absolute (signed media) URL, without the bearer token.
    pub async fn fetch_text(&self, url: &str) -> ApiResult<String> {
        let response = self.send(self.http.get(url)).await?;
        response.text().await.map_err(|e| ApiFailure {
            status: None,
            code: "decode".into(),
            message: format!("Couldn't read {url} ({}).", root_cause(&e)),
        })
    }

    /// `ws(s)://…/api/events` for this service.
    pub fn events_url(&self) -> String {
        let url = self.url(routes::EVENTS);
        if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            url
        }
    }
}

async fn failure_from(response: Response) -> ApiFailure {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<ApiError>(&text) {
        Ok(err) => ApiFailure {
            status: Some(status.as_u16()),
            code: err.code,
            message: err.message,
        },
        Err(_) => ApiFailure {
            status: Some(status.as_u16()),
            code: if status == StatusCode::UNAUTHORIZED {
                "unauthorized".into()
            } else {
                "http".into()
            },
            message: match status {
                StatusCode::UNAUTHORIZED => "This device is no longer signed in.".into(),
                StatusCode::NOT_FOUND => "The service doesn't know that item any more.".into(),
                s if s.is_server_error() => {
                    format!("The service had a problem ({}).", s.as_u16())
                }
                s => format!("The service refused the request ({}).", s.as_u16()),
            },
        },
    }
}

fn root_cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut current = err;
    while let Some(next) = current.source() {
        current = next;
    }
    current.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FakeService, sample_library};
    use margin_shared::{Level, ProposalStatus, SearchKind};

    #[tokio::test]
    async fn sign_in_flow_pending_then_approved() {
        let fake = FakeService::start().await;
        let api = Api::anonymous(&fake.url);
        api.health().await.unwrap();
        let login = api.start_login("test-mac").await.unwrap();
        assert_eq!(fake.state.lock().unwrap().device_name, "test-mac");
        assert!(login.authorize_url.starts_with("https://github.com/"));
        assert_eq!(api.poll_login(&login).await.unwrap(), LoginState::Pending);
        fake.state.lock().unwrap().approve = true;
        match api.poll_login(&login).await.unwrap() {
            LoginState::Approved {
                token,
                github_login,
            } => {
                assert_eq!(token, crate::fake::TOKEN);
                assert_eq!(github_login, "Nkr1shna");
            }
            other => panic!("expected approval, got {other:?}"),
        }
        // Only once.
        assert_eq!(api.poll_login(&login).await.unwrap(), LoginState::Expired);
    }

    #[tokio::test]
    async fn library_and_crud_with_bearer() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let lib = api.library().await.unwrap();
        assert_eq!(lib, sample_library());
        let me = api.me().await.unwrap();
        assert_eq!(me.github_login, "Nkr1shna");
        assert!(me.device.current);

        let topic = api
            .create_topic(&NewTopic {
                parent_id: None,
                title: "Linear Algebra".into(),
            })
            .await
            .unwrap();
        assert_eq!(topic.title, "Linear Algebra");
        let renamed = api
            .patch_topic(
                &topic.id,
                &TopicPatch {
                    title: Some("Linear algebra".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(renamed.title, "Linear algebra");
        api.delete_topic(&topic.id).await.unwrap();
        let err = api.delete_topic(&topic.id).await.unwrap_err();
        assert_eq!(err.status, Some(404));
        assert_eq!(err.code, "not_found");

        let note = api
            .create_note(&NewNote {
                topic_id: "fourier".into(),
                lesson_id: Some("l1".into()),
                title: "Gibbs".into(),
                body_md: "9%".into(),
                quote: Some("overshoot".into()),
            })
            .await
            .unwrap();
        let patched = api
            .patch_note(
                &note.id,
                &NotePatch {
                    body_md: Some("about 9%".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(patched.body_md, "about 9%");
        assert_eq!(patched.title, "Gibbs");
        api.delete_note(&note.id).await.unwrap();
    }

    #[tokio::test]
    async fn unauthorized_without_token() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, "wrong");
        let err = api.library().await.unwrap_err();
        assert!(err.is_unauthorized());
        assert_eq!(err.message, "Sign in again.");
    }

    #[tokio::test]
    async fn ask_reports_no_api_key_and_accepts_outline() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let req = AskRequest {
            prompt: "Fourier series".into(),
            lesson_count: 3,
            level: Level::Intro,
            with_videos: true,
            with_quiz: true,
            parent_topic_id: None,
            source_ids: vec![],
        };
        fake.state.lock().unwrap().has_api_key = false;
        let err = api.ask(&req).await.unwrap_err();
        assert!(err.is_no_api_key());
        fake.state.lock().unwrap().has_api_key = true;
        let proposal = api.ask(&req).await.unwrap();
        assert_eq!(proposal.status, ProposalStatus::Drafting);
        let fetched = api.proposal(&proposal.id).await.unwrap();
        assert_eq!(fetched.status, ProposalStatus::Ready);
        let topic = api
            .accept_proposal(
                &proposal.id,
                &AcceptProposal {
                    outline: fetched.outline.unwrap(),
                },
            )
            .await
            .unwrap();
        assert_eq!(topic.title, "Fourier series");
    }

    #[tokio::test]
    async fn search_encodes_query() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let hits = api.search("sines & cosines").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, SearchKind::Lesson);
        assert_eq!(hits[0].id, "l1");
        assert!(hits[0].snippet.contains("sines & cosines"));
    }

    #[tokio::test]
    async fn settings_tools_devices_and_video() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let settings = api
            .patch_settings(&SettingsPatch {
                openai_api_key: Some("sk-test".into()),
                render_fps: Some(60),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(settings.has_api_key);
        assert_eq!(settings.render_fps, 60);
        assert_eq!(api.settings().await.unwrap(), settings);
        let tools = api.tools().await.unwrap();
        assert!(
            tools
                .iter()
                .any(|t| !t.found && !t.install_commands.is_empty())
        );
        let devices = api.devices().await.unwrap();
        assert_eq!(devices.len(), 2);
        api.revoke_device(&devices[1].id).await.unwrap();
        assert_eq!(api.devices().await.unwrap().len(), 1);
        let video = api.make_video("l1").await.unwrap();
        assert_eq!(video.status, margin_shared::VideoStatus::Queued);
        assert_eq!(api.video(&video.id).await.unwrap().id, video.id);
        let lesson = api.regenerate_lesson("l1").await.unwrap();
        assert_eq!(lesson.status, margin_shared::LessonStatus::Queued);
    }

    #[tokio::test]
    async fn sources_upload_with_query_and_attach_on_accept() {
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let source = api
            .upload_source(
                b"# Gibbs & friends\n".to_vec(),
                "notes/ch 1.md",
                Some("Fourier notes"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(source.name, "notes/ch 1.md");
        assert_eq!(source.folder.as_deref(), Some("Fourier notes"));
        assert_eq!(source.topic_id, None);
        assert_eq!(source.kind, margin_shared::SourceKind::Text);
        assert_eq!(source.bytes, 18);
        assert_eq!(source.status, margin_shared::SourceStatus::Queued);

        let in_topic = api
            .upload_source(b"fn main() {}".to_vec(), "main.rs", None, Some("fourier"))
            .await
            .unwrap();
        assert_eq!(in_topic.topic_id.as_deref(), Some("fourier"));
        assert!(api.library().await.unwrap().sources.contains(&in_topic));
        let err = api
            .upload_source(b"x".to_vec(), "archive.zip", None, None)
            .await
            .unwrap_err();
        assert_eq!(err.status, Some(400));

        // Accepting a proposal that names the source attaches it to the new topic.
        let req = AskRequest {
            prompt: "Fourier series".into(),
            lesson_count: 1,
            level: Level::Intro,
            with_videos: false,
            with_quiz: false,
            parent_topic_id: None,
            source_ids: vec![source.id.clone()],
        };
        let proposal = api.ask(&req).await.unwrap();
        let outline = api.proposal(&proposal.id).await.unwrap().outline.unwrap();
        let topic = api
            .accept_proposal(&proposal.id, &AcceptProposal { outline })
            .await
            .unwrap();
        let lib = api.library().await.unwrap();
        let attached = lib.sources.iter().find(|s| s.id == source.id).unwrap();
        assert_eq!(attached.topic_id.as_deref(), Some(topic.id.as_str()));

        api.delete_source(&source.id).await.unwrap();
        assert_eq!(
            api.delete_source(&source.id).await.unwrap_err().status,
            Some(404)
        );
    }

    #[tokio::test]
    async fn upload_refuses_files_over_the_limit_locally() {
        let api = Api::with_token("http://127.0.0.1:9", "x");
        let big = vec![0u8; routes::MAX_SOURCE_BYTES as usize + 1];
        let err = api
            .upload_source(big, "big.txt", None, None)
            .await
            .unwrap_err();
        assert_eq!(err.code, "local");
        assert!(err.message.starts_with("big.txt is larger than 50 MB"));
    }

    #[tokio::test]
    async fn embedding_model_download_and_delete() {
        use margin_shared::EmbeddingModelState;
        let fake = FakeService::start().await;
        let api = Api::with_token(&fake.url, crate::fake::TOKEN);
        let model = api.embedding().await.unwrap();
        assert_eq!(model.repo, "FluidInference/embeddinggemma-2-coreml");
        assert!(matches!(
            model.state,
            EmbeddingModelState::NotDownloaded { .. }
        ));
        let downloading = api.download_embedding().await.unwrap();
        assert!(matches!(
            downloading.state,
            EmbeddingModelState::Downloading { received: 0, .. }
        ));
        assert_eq!(api.embedding().await.unwrap(), downloading);
        let removed = api.delete_embedding().await.unwrap();
        assert!(matches!(
            removed.state,
            EmbeddingModelState::NotDownloaded { .. }
        ));
    }

    #[tokio::test]
    async fn network_failure_has_readable_message() {
        let api = Api::anonymous("http://127.0.0.1:9");
        let err = api.health().await.unwrap_err();
        assert_eq!(err.code, "network");
        assert!(
            err.message
                .starts_with("Couldn't reach the service at http://127.0.0.1:9")
        );
    }

    #[test]
    fn events_url_uses_ws_scheme() {
        assert_eq!(
            Api::anonymous("https://m.example/").events_url(),
            "wss://m.example/api/events"
        );
        assert_eq!(
            Api::anonymous("http://127.0.0.1:8787").events_url(),
            "ws://127.0.0.1:8787/api/events"
        );
    }
}
