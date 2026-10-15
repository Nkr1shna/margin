# Margin

Margin is a "Live context" app for a company's projects: people work through a
voice-first GUI (desktop, iPhone, Android), agents get the same knowledge as context
(MCP and the context API), with learning (courses, lessons, quizzes) built in.

## Crates

| Crate | What it is |
|---|---|
| `crates/service` | The service: axum with embedded SurrealDB, GitHub sign-in, generation jobs, the live event socket. Runs on Linux (an Ubuntu VM per environment). |
| `crates/app` | The app (Dioxus): desktop, iOS and Android, a thin live client of the service. |
| `crates/shared` | The contract between service and apps: records, request bodies, events, routes. |
| `crates/agent` | The chat agent loop: model calls and tools. |
| `crates/ai` | Text generation behind one seam (OpenAI and Claude providers). |
| `crates/embed` | Local embeddings (EmbeddingGemma on ONNX Runtime) and source readers. |
| `crates/video` | Narrated lesson videos (ManimGX, PocketTTS). |
| `crates/evals` | The eval suite and the `margin-eval` runner. |
| `crates/mcp` | `margin-mcp`: Claude Code's MCP server over the context API. |

## Build and test

```sh
MARGIN_EMBED_MODEL_DIR=/nonexistent cargo test --workspace   # tests run without the model
cargo clippy --workspace --all-targets -- -D warnings
sh deploy/local/linux-check.sh                               # CI's Linux steps, in a Tart VM
```

The app is built with `dx` (Dioxus CLI 0.7.9), run from the repository root:
`dx serve -p margin-app`, or `dx bundle -p margin-app --platform desktop --package-types macos --release`
for the Mac app. Setting up a service: `docs/service-setup.md`.

## Branches and releases

Work goes to a feature branch and a PR squash-merged into `dev`, which deploys the dev
service. Prod ships by promoting a dev snapshot into `release` (merge commit), which
deploys prod and tags the release. `main` is unused. What shipped when:
`docs/RELEASES.md`; how: `docs/plan-deploy.md` §4a.

## Where to read next

- [Linear project](https://linear.app/krishnanellooremargin/project/margin-fc714ebf8f0b): the roadmap, what's in flight and next.
- `docs/later.md`: slices and ideas with no Linear issue yet.
- `docs/HANDOFF.md`: the build log and the docs to read first.
