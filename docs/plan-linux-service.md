# Linux service plan: inference on Linux VMs, the Mac kept

Written 2026-10-14 at `95393bd` on `agent/linux-plan`. Read: `docs/plan-deploy.md`, `docs/plan-live-context.md` (Q16, §8.1), `docs/agent-rag-architecture.md` (§2.0 stages, §2.1, §2.2, §3), `crates/embed/**`, `crates/service/src/{secrets,engines,video,sources,ingest,config,main}.rs`, `crates/video/**`, `crates/evals/src/{live.rs,bin/margin-eval.rs}`, `evals/{variants,gates.toml}`. Checked on the web today: the EmbeddingGemma 2 model card and its ONNX, GGUF, Core ML and MLX conversions, `ort` 2.0.0-rc.14 and its prebuilt-binary table, `fastembed` 7.1.1, `parakeet-rs` 0.4.1, `pdfium-render` 0.9.4 and `pdfium-binaries` chromium/8100, `manimgx` 0.1.1 and `pocket-tts` 3.3.0 on PyPI. A feasibility spike ran on this Mac (§8). Where a fact is unverified it says so.

Owner (verbatim): "convert the inference setup from macs to linux boxes so the inference runs on it and we can remove these limits, These are small models running on the vm so it shouldn't be such an issue" and "we can use the appropriate models non mlx and non core ml to run them it's fine." So: a Linux build of `margin-service` whose embedder, readers, speech, secrets, sandbox and process manager have Linux backends behind the traits that already exist; the macOS backends stay and are selected by target (and, where both can build, by config); vectors on Linux differ from the Mac's and the service re-embeds through `embedder_version`.

**Why.** `docs/plan-deploy.md` §1: Apple's Virtualization framework allows two macOS guests per host, macOS guests have no Neural Engine, and dev + prod take both slots, so CI shares the host. Linux guests under Tart have no such limit, so dev, prod and a Linux CI runner can each be a VM, and the Mac mini's macOS guests are released.

---

## 0. Decisions in one table

| Piece | macOS today | Linux backend (chosen) | Behind |
|---|---|---|---|
| Embedding (text, image, audio) | EmbeddingGemma 2 Core ML (`FluidInference/embeddinggemma-2-coreml`, fp16, ANE/GPU) | EmbeddingGemma 2 ONNX (`onnx-community/embeddinggemma-2-ONNX`, q8) on **`ort` 2.0.0-rc.14** (ONNX Runtime 1.31, CPU EP, prebuilt `aarch64-unknown-linux-gnu`) | `Embedder`, `SourceTools::load` |
| PDF text + page render | PDFKit | **`pdfium-render` 0.9.4** + `pdfium-linux-arm64` from `bblanchon/pdfium-binaries` (BSD-3) | `SourceTools::{extract,extract_lazy,pdf_page_picture}` |
| OCR (scanned pages, images) | Vision `VNRecognizeTextRequest` | **`tesseract` 5 CLI** (apt), detected like `manimgx`; `ocrs` only if the owner prefers zero system packages (Q4) | `extract` |
| Image decode, HEIC, resize, JPEG for the model | ImageIO | **`image` 0.25** (+ EXIF orientation) and **`libheif-rs` 3.0** (LGPL-3 dynamic) for HEIC | `extract`, `SourceTools::image_for_model`, `jpeg_for_model` |
| Audio/video decode, frames | AVFoundation | **`ffmpeg` / `ffprobe` CLI** (apt, LGPL build) | `extract` (`av.rs` sibling) |
| Speech to text | `fluidaudiocli transcribe` (FluidAudio Parakeet, Core ML) | **`parakeet-rs` 0.4.1** (Parakeet TDT ONNX, in-process, same `ort`); `whisper-rs` is the fallback | `SourceTools::transcribe` |
| Narration (video lessons) | `fluidaudiocli tts --backend pocket` | **`pocket-tts`** (Kyutai, Python, CPU) behind a small CLI the Tools pane installs; Piper if the licence fails (Q7) | `margin_video` voice step |
| Scene render sandbox | `sandbox-exec` profile | **`bubblewrap`** (`bwrap`: no net, read-only root, writes in `work_dir` + tmpfs) | `SandboxMode` |
| Secrets | login Keychain via `keyring` | **File store** `secrets/<name>` 0600 in a 0700 dir (Secret Service needs an unlocked desktop session; kernel keyutils forgets on reboot) | `SecretStore` |
| Process manager | LaunchAgent in a logged-in session | **systemd** system service (`margin` user, `/var/lib/margin`, journald) | `deploy/` |
| Config/data paths | `~/Library/Application Support/Margin` | `/etc/margin/service.toml`, `/var/lib/margin` (override `MARGIN_CONFIG`/`data_dir` as today) | `config.rs` |

Rejected for embedding: **llama.cpp** (`ggml-org/embeddinggemma-2-GGUF` has text GGUFs and an `mmproj`, text works since PR #30054, but multimodal *embedding output* through `mtmd` is unverified upstream and Unsloth's guide documents text/code only); **candle** (no EmbeddingGemma 2 model; it would mean porting the Gemma 3-style text tower and the Gemma 4 vision/audio towers by hand); **fastembed** (an `ort` wrapper with `embeddinggemma-300m` only, text only; adds nothing over `ort` directly). `ort` wins because the official ONNX export covers all three modalities, the prebuilt CPU binaries cover `aarch64-unknown-linux-gnu`, `parakeet-rs` pins the same `ort` version, and the spike ran.

---

## 1. What is macOS-only today (inventory)

- `crates/embed`: `coreml.rs`, `vision.rs`, `audio.rs` (Core ML), `av.rs` (AVFoundation), `pdf.rs` (PDFKit + Vision OCR for pages), `image.rs` (ImageIO + Vision OCR), `media_tests.rs`; `extract.rs` has `cfg(not(macos))` stubs that return `Extract("… macOS only")`; `lib.rs` `pdf_page_picture`/`jpeg_for_model` stubs; `transcript.rs` shells out to `fluidaudiocli` (Swift, macOS). The Core ML pipeline is single-sequence (`embed_S` functions), so "batching" is pipelining.
- `crates/service`: `secrets::KeychainStore` (`keyring` 4.2, Apple store); `engines::EmbedTools::load` is `cfg(macos)` → `CoreMlEmbedder`, else `Err`; `config.rs` default paths under `~/Library/Application Support/Margin`; `video.rs` `SandboxMode::MacosSandboxExec`; `main.rs` help text.
- `crates/video`: `sandbox.rs` (`sandbox-exec` profile with Metal/IOKit allowances), `tools.rs` (`xcode-select`, Swift build lines for `fluidaudiocli`), `lib.rs` PocketTTS through `fluidaudiocli`.
- `crates/evals`: `live::LiveVectors` and `margin-eval`'s live commands are `cfg(macos)` ("needs the service Mac"); `sw_vers` in the report.
- Deploy: LaunchAgents, `codesign`, Keychain prompts (`docs/plan-deploy.md`).
- Not affected: `crates/app` and `crates/mcp` (device side; `keyring` on a Linux desktop uses Secret Service, which is right for a GUI client), SurrealDB (`kv-surrealkv`), axum, the agent loop, providers.

---

## 2. The Linux backends

### 2.1 Embedding: EmbeddingGemma 2 ONNX on `ort`

**Model facts (checked).** `google/embeddinggemma-2`: 740M parameters in all (text 270M = 130M backbone + 140M embedder; vision encoder 170M; audio encoder 300M), 768-d output from a 512→768 projection, mean pooling, L2-normalised, Matryoshka 512/256/128, context 8 192 tokens, bf16 safetensors; "do not use float16" (NaNs). Prompts are the ones Margin already uses (`task: search result | query: …`, `title: {title} | text: …`, `title: none`). Images cost 280 soft tokens by default (70–1 120), video frames 140, audio 25 tokens/s at 16 kHz mono. Model-card header says **Apache-2.0**, but the licence link goes to a Gemma licence page and deployments "must adhere to the Gemma Prohibited Use Policy" (Q1 for the owner; it is the same model the Mac runs today, so nothing changes legally by switching runtime).

`onnx-community/embeddinggemma-2-ONNX` (same licence header) holds, under `onnx/`, five dtypes × three graphs with external data: `model{,_fp16,_q4,_q4f16,_quantized}.onnx(+_data)` 1 085 / 543 / 175 / 157 / **314 MB**; `vision_encoder*` 671 / 336 / 109 / 98 / **195 MB**; `audio_encoder*` 1 173 / 587 / 189 / 171 / **340 MB**; plus `tokenizer.json` (32 MB, the Gemma tokenizer; `tokenizers` 0.23.2 loads it, already a dependency), `config.json`, `processor_config.json`, `preprocessor_config.json`. The card's quality table: fp16 worst-case cosine 0.9998 vs fp32, q8 not separately stated, q4 0.975 (text-only 0.988), audio suffers most from 4-bit. **Choice: q8 (`*_quantized`) for all three graphs** (850 MB total, against 1.4 GB of Core ML today); fp32 as a config option for the eval A/B.

**Graph shapes (dumped by the spike).** Text graph: inputs `input_ids` i64 `[batch, seq]`, `attention_mask` i64 `[batch, seq]`, `image_features` f32 `[n_image_tokens, 512]`, `video_features` f32 `[n, 512]`, `audio_features` f32 `[n, 512]`; outputs `last_hidden_state` `[batch, seq, 768]` and `sentence_embedding` `[batch, 768]` (already normalised: the spike's vectors have norm 1.0000). So the text graph does the work `coreml.rs` does by hand today (token table lookup, scaling, pooling, projection, normalisation), and media go in as 512-wide feature rows at the positions of the image/audio placeholder tokens (`config.json`: `boi_token_id` 255999, `image_token_id` 258880, `eoi_token_id` 258882, `audio_token_id` 258881, `boa` 256000, `eoa` 258883), the same shape as `coreml.rs`'s `<bos> <|image> … <image|> <eos>` wrapping. The vision and audio encoders' input names were **not dumped** (the spike loaded the text graph only); the first task of the media area is to dump them and write them into `onnx/mod.rs` as constants.

**Preprocessing (from `processor_config.json`).** Image: rescale 1/255, no mean/std normalisation, patch 16, pooling kernel 3, 280 soft tokens (`ImageTokens::High`; `Medium` 140 for frames as today, `Low` 70). The exact resize rule (the card gives no pixel size; 280 tokens × 9 patches × 256 px² suggests ~ 896×896 for a square image) must be read from the Transformers.js processor code and checked against the HF processor on three images before anything is embedded (the same validation `vision.rs` had against FluidUse). Audio: `Gemma4AudioFeatureExtractor`, 16 kHz, 128 mel bins, FFT 512, window 320, hop 160, 0–8 000 Hz, mel floor 0.001, no pre-emphasis/dither, right padding, 40 ms per token, 280 tokens max (11.2 s), which fits Margin's `WINDOW_SAMPLES` (10 s = 250 tokens, exactly the Core ML card's "250 tokens per 10 s window"). The mel front end is reimplemented in Rust (`realfft`, as `parakeet-rs` does for its own front end) and unit-tested against features recorded from the Python extractor on the `evals/corpus` audio clip (a small `.bin` fixture under `crates/embed/tests/fixtures/`).

**Rust shape** (`crates/embed/src/onnx/{mod,text,vision,audio,mel}.rs`, `cfg(any(target_os = "linux", feature = "onnx"))`):

```rust
pub struct OnnxEmbedder(onnx::Model);            // lib.rs, beside CoreMlEmbedder
impl OnnxEmbedder { pub fn load(files: &ModelFiles, opts: &OnnxOptions) -> Result<Self, EmbedError>; }
pub struct OnnxOptions { pub dtype: OnnxDtype /* Q8 | Fp32 | Fp16 */, pub threads: usize, pub batch_tokens: usize /* ≤ 8 192, default 4 096 */ }
impl Embedder for OnnxEmbedder { /* embed, count_tokens, embed_images, embed_audio, embed_pipelined */ }
```

- One `Session` per graph behind a `Mutex` (`Session::run` takes `&mut self` in rc.14; concurrent runs are unsafe). Encoders load on first use and are dropped after `ENCODER_IDLE` (10 min) so the text-only RSS stays small (§3).
- **Real batching.** The graph is `[batch, seq]`: `embed` sorts inputs by token count, fills batches up to `batch_tokens` tokens with right padding (`attention_mask` 0), runs, and restores order. `embed_pipelined` keeps the `pipelined` helper (tokenise on one thread, run on another) with `batch` inputs per `on_batch`, which is what `ingest.rs` writes per statement (`EMBED_BATCH` 64).
- **Execution provider.** Explicit: `Session::builder(&env)?.with_auto_device(AutoDevicePolicy::PreferCPU)?.with_execution_providers([ep::CPU::default().build()])?.with_intra_threads(threads)`. Spike finding: rc.14's builder sets the ONNX Runtime auto-device policy to *MaxEfficiency* by default, which on macOS picked the CoreML EP for parts of this graph and failed on the empty `[0, 512]` media inputs ("has zero elements. This is not supported by the CoreML EP"); `with_execution_providers` alone did not override it, `with_auto_device(PreferCPU)` did. On Linux the CPU EP is the only one in the prebuilt binary, but the call stays so the Mac build of the ONNX backend (used for the eval A/B, §4) behaves the same.
- **Linking.** `ort` default = download pyke's static binaries (`ort-sys` `dist.tsv` lists `aarch64-unknown-linux-gnu` CPU, `aarch64-apple-darwin` with CoreML, no musl). Linux binaries are built with Clang and depend on `libc++` → the guest installs `libc++1`; the service binary links `libc++` dynamically (the provisioning script's package list, §5). If pyke's CDN is unwanted in CI, `ORT_LIB_LOCATION` points at Microsoft's `onnxruntime-linux-aarch64-1.31.0.tgz` checked into the runner's cache (`load-dynamic` feature); decide in LX-deploy, default is the static download.
- **CPU features.** ONNX Runtime's MLAS uses NEON and `dotprod` for int8 GEMM; `i8mm` (Armv8.6) is not on the M1, so q8 on the M1 guest runs the `dotprod` kernels. Unverified until measured there (§3).
- **Download.** `download.rs` grows a per-backend manifest: `Backend::CoreMl` (today's `REPO`, packages, tables) and `Backend::Onnx { dtype }` (`onnx-community/embeddinggemma-2-ONNX`: `tokenizer.json`, `config.json`, `processor_config.json`, `preprocessor_config.json`, the six `onnx/<graph>_<dtype>.onnx{,_data}` files), stored under `data_dir/models/embeddinggemma-2-onnx-<dtype>/`; `ModelFiles { dir, backend }`; `is_complete`/`remote_size`/`download`/`remove` take the manifest from `backend`. `Settings → Sources` shows the backend's name and size (ONNX q8: ~0.9 GB).
- **Version.** `EMBEDDER_VERSION` becomes `fn embedder_version(backend) -> &'static str`: `embeddinggemma-2-coreml/title-prefix-v1` (unchanged) and `embeddinggemma-2-onnx-q8/title-prefix-v1` (and `…-fp32…`). `ingest.rs` already re-embeds rows whose `embedder` differs (diff rule) and stamps `embedder` per row; `Recording` carries the version and `RecordedEmbedder` refuses a mismatch; `margin-eval compare` refuses across versions without `--allow-embedder-change`. All of that is why no second migration mechanism is needed.

**Will the vectors match the Mac's?** Not exactly, and they must not be mixed. The Core ML text model is fp16 with cosine ≥ 0.9984 (mean 0.99996) to PyTorch fp32 (its card); the ONNX q8 text model is at cosine 0.9999 to the ONNX fp32 export in the spike and fp16 is ≥ 0.9998 per its card; so text vectors across the two backends are expected at ≥ 0.998 cosine (inferred, not measured: the Core ML model was not loaded in the spike). Audio differs more (Core ML fp16 audio window cosine ≥ 0.977 to PyTorch), and OCR text (Vision vs tesseract) changes chunk *texts*, not just vectors. The owner has accepted a re-embed, so:

- **Switch = re-embed, one pass.** At first start of a build whose active embedder version differs from the rows', the ingest worker queues every `Ready` document (oldest first, the same loop as `passages-v1`; `SettingsRow.migrations` records `embedder-<version>`), with progress in Settings → Sources ("Re-indexing 212 of 1 480 documents"). Until the pass completes the **dense leg** adds `AND embedder = $embedder` (one more pre-filter conjunct inside the `KnnScan`, like `access`); the BM25 leg is unaffected; `search::rrf` keeps working with a thinner dense list. After the pass the conjunct stays (it is cheap and makes a half-migrated backup safe). Retrieval traces record `embedder`, so the eval runner can tell which vectors answered.
- **Chunker unchanged**, so `chunk_hash` rows are kept and only `vector`/`embedder` are rewritten (`db.rs` 642: the update that "rewrites everything but the text, hash, embedder and vector" gains the inverse, `rewrite_vectors`). PDF pages whose text came from OCR do change text when the OCR engine changes: those documents are re-extracted (a `Document.extractor` stamp, `"pdfkit+vision"` vs `"pdfium+tesseract"`, decides; new field, additive).
- Recordings: `evals/recorded/embeddings-<version>.bin` per backend (§4).

### 2.2 PDF: `pdfium-render` + pdfium

`pdfium-render` 0.9.4 (MIT/Apache-2.0) binds pdfium dynamically (`Pdfium::bind_to_library(path)`). The library is `pdfium-linux-arm64.tgz` (3.7 MB, chromium/8100, BSD-3-Clause; also `pdfium-linux-musl-arm64` and `pdfium-mac-arm64` for the Mac build of the ONNX variant) installed by the provisioning script to `/usr/local/lib/libpdfium.so`, searched in `data_dir/tools/` first so a user install needs no root. `pdf_pdfium.rs` keeps `pdf.rs`'s contract: `extract(path, media_dir)` → pages with `number`, text from `PdfPage::text().all()` with the same `non_space_chars` density check → `ocr_page` when the text layer is empty (render at 200 dpi → tesseract); `page_picture(path, n, out)` renders at the resolution the Mac path uses (`render` in `pdf.rs`: the longest side capped as today) into PNG through `image`. MuPDF is AGPL and is not used; `pdf-extract`/`lopdf` have no renderer.

### 2.3 OCR: tesseract

`tesseract` 5 from apt (`tesseract-ocr`, `tesseract-ocr-eng`; Apache-2.0) run as a subprocess, `tesseract <png> stdout --psm 3 -l eng`, the way `fluidaudiocli` is run (`transcript.rs`): no C bindings (`leptess` 0.14 is from 2023; `tesseract-rs` needs `libtesseract-dev` + bindgen on the runner). Detected in `Settings → Tools` with its install line, like `manimgx`; without it, images and scanned pages ingest with empty text and the warning the source already shows for a missing transcriber (`sources::transcript_warning` gets an OCR sibling). Vision OCR on the Mac is better on photos and handwriting than tesseract; the cross-modal queries in `evals/golden/retrieval.jsonl` measure the gap (§4). `ocrs` 0.13 (pure Rust, Apache/MIT, its own ~10 MB models) is the no-system-packages alternative with weaker accuracy on dense pages; Q4.

### 2.4 Images

`image` 0.25 decodes PNG/JPEG/WebP/GIF/TIFF/BMP; EXIF orientation via `kamadak-exif` (what ImageIO applied implicitly); `libheif-rs` 3.0 for HEIC/HEIF (libheif LGPL-3 + libde265, dynamic, apt `libheif1`/`libheif-dev`; behind a `heic` cargo feature on by default for Linux, so a CI machine without the dev package can still build with `--no-default-features`). `jpeg_for_model(path, max_side)` and `image_for_model` resize with `FilterType::Triangle` and encode JPEG at the quality `image.rs` uses. `Format`/`write` for pages and frames go through `image` too.

### 2.5 Audio and video: ffmpeg CLI

`ffmpeg`/`ffprobe` from apt (LGPL build; the Debian/Ubuntu `ffmpeg` package is GPL-enabled, which is fine for a subprocess). `av_ffmpeg.rs` mirrors `av.rs`: `sound(path)` = `ffmpeg -v error -i <path> -vn -ac 1 -ar 16000 -f f32le -` read from stdout (bounded by `MAX_UPLOAD` × 16 kHz × 4 B); `duration_ms` and `has_video` from `ffprobe -v error -show_entries format=duration:stream=codec_type -of json`; `frames(path, times)` = one `ffmpeg -ss <t> -i <path> -frames:v 1 -vf scale='min(1280,iw)':-2 <frame.png>` per time from `frame_times` (≤ 120 per video, as today), run through `tokio::process` with the service's existing streaming helper. `ffmpeg-next` bindings are not used (they pin an FFmpeg major and need headers on every builder).

### 2.6 Speech to text: Parakeet on `ort`

`parakeet-rs` 0.4.1 (updated 2026-10-13; depends on `ort = "=2.0.0-rc.14"`, `ndarray 0.17`, `tokenizers 0.23.2`, `realfft`, `hound`), i.e. the same runtime and the same Parakeet TDT family FluidAudio runs on the Mac, so transcripts are comparable and word timestamps exist for `Transcript::chunks`. `transcript_parakeet.rs` implements `SourceTools::transcribe(samples, work_dir)` in-process (no WAV round trip); the model (NVIDIA Parakeet TDT 0.6B v3, CC-BY-4.0; the exact ONNX repo id `parakeet-rs` expects is **unverified**, read its README in the area) downloads like the embedder, under `data_dir/models/parakeet-tdt-0.6b-v3/`, shown in Settings → Sources with its size (~600 MB fp32; check for an int8 export). `whisper-rs` 0.16 (whisper.cpp, MIT; `large-v3-turbo` ggml) is the fallback if `parakeet-rs` turns out immature in the spike the area starts with (its 0.4.x line is three days old at writing); it is slower on CPU and has no word timestamps without extra work. Diarization is not needed (meetings record per participant, live-context plan §5.2).

### 2.7 Narration for video lessons

`fluidaudiocli tts --backend pocket` is Swift/Core ML. The same model exists as Kyutai's `pocket-tts` 3.3.0 (Python ≥ 3.10, `torch ≥ 2.5` CPU, ~2 cores, ~6× real time on an M4 Air per its README; licence of the package and of the voices **not stated on PyPI**: Q7). A 60-line `margin-tts` script (`uv tool install`, like `manimgx`) exposes `margin-tts --text … --output … [--voice …]` and `lib.rs`'s `pocket_tts` calls whichever of `fluidaudiocli`/`margin-tts` `tools.rs` found. Piper (ONNX, MIT, `piper-rs`) is the fallback with different voices.

### 2.8 Secrets: a file store

`secrets::FileStore { dir }` writes `data_dir/secrets/<name>` (dir 0700, files 0600, write to `<name>.tmp` + `rename`, refuse if the dir is group/world readable), same names (`github-client-secret`, `openai-api-key`, `anthropic-api-key`, `media-signing-key`, `elevenlabs-api-key`). Selected by `config.secrets = "keychain" | "file"` (default: `keychain` on macOS, `file` on Linux); `setup` prints "stored in `<dir>`" instead of "in the Keychain", and the three user-facing strings that say "Keychain" (`auth.rs`, `state.rs`, `shared`) become "the service's secret store". Why not Secret Service: `keyring`'s Linux backends need a D-Bus session with an unlocked `gnome-keyring`/KWallet (a desktop login), which a headless systemd service has not; the kernel keyring (`keyutils`) is per-session and empties on reboot. Hardening on top is systemd's (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/margin`, `PrivateTmp`, `NoNewPrivileges`), and optionally `LoadCredentialEncrypted=` for the GitHub client secret (Q5). Devices are unchanged: keys never leave the service.

### 2.9 Scene sandbox: bubblewrap

`SandboxMode::LinuxBubblewrap`; `sandbox_linux.rs::sandbox_args(program, work_dir)` builds:

```
bwrap --unshare-all --die-with-parent --new-session
      --ro-bind /usr /usr --ro-bind /etc /etc --symlink usr/lib /lib --symlink usr/lib64 /lib64 (as present)
      --ro-bind <manimgx tool dir> <same>          # the uv tool venv, read-only
      --ro-bind <fonts/cache dirs> <same>
      --bind <work_dir> <work_dir> --tmpfs /tmp --dev /dev --proc /proc
      --setenv HOME <work_dir> --setenv TMPDIR /tmp --clearenv (then the few vars manimgx needs)
      --chdir <work_dir> -- <manimgx> …
```

`--unshare-all` includes `--unshare-net` (no network, the property `sandbox.rs` tests assert) and a new PID namespace (so `process-fork` is allowed but confined; scenes can start nothing outside the bind set). manimgx 0.1.1 ships `manylinux_2_28` aarch64 wheels with Mesa **lavapipe** bundled for CPU Vulkan, so no `/dev/dri` is bound; rendering 1080p60 on lavapipe may be far slower than Metal — the LX-video area measures the `evals` fixture scene on the dev guest first and reports (risk §7). Landlock (`landlock` 0.4) is not added: bwrap already confines files and network; two sandboxes are two things to debug. `bwrap` comes from apt (`bubblewrap`, LGPL-2+); unprivileged user namespaces must be enabled in the guest (Ubuntu 24.04 has AppArmor restrictions on unprivileged userns: the provisioning script installs the `bwrap` AppArmor profile or sets `kernel.apparmor_restrict_unprivileged_userns=0`; Q6). `RenderError::Sandbox` when `bwrap` is missing, as on macOS without `sandbox-exec`.

### 2.10 systemd, paths, tools

- `deploy/systemd/margin-service.service` (system unit, `User=margin`, `Environment=MARGIN_CONFIG=/etc/margin/service.toml`, `ExecStart=/usr/local/bin/margin-service serve`, `Restart=always`, `MemoryMax=` from §3, hardening above but **no** `SystemCallFilter` that blocks `unshare`/`clone` namespaces, which bwrap needs), `margin-backup.service` + `.timer` (nightly), `cloudflared` via its own `service install`.
- `config.rs`: `config_path()` = `MARGIN_CONFIG` → `/etc/margin/service.toml` (Linux) / today's path (macOS); `default_data_dir()` = `/var/lib/margin` / `~/Library/Application Support/Margin`; nothing else keyed on the OS. `main.rs` help text per OS.
- `video/tools.rs`: install commands per OS (`apt install …`, `uv tool install manimgx`, `margin-tts`); `command_line_tools_installed` only on macOS; `tesseract`, `ffmpeg`, `bwrap`, `libpdfium.so` detected and listed in Settings → Tools with the same `ToolStatus` rows (new tool names; the pane renders rows generically).
- `evals`: `LiveVectors` loads `OnnxEmbedder` or `CoreMlEmbedder` by `--embedder`; the "needs the service Mac" bail goes; `os_version()` reads `/etc/os-release` on Linux.

### 2.11 Build and selection

`crates/embed/Cargo.toml`:

```toml
[features]
default = []
onnx = ["dep:ort", "dep:ndarray", "dep:realfft"]                 # also buildable on macOS, for the A/B
linux-readers = ["dep:pdfium-render", "dep:image", "dep:kamadak-exif"]
heic = ["dep:libheif-rs"]
speech-onnx = ["dep:parakeet-rs"]
[target.'cfg(target_os = "linux")'.dependencies]  # the same crates, non-optional, so a plain Linux build has everything
```

`engines::EmbedTools::load` picks by `config.embedder = "coreml" | "onnx-q8" | "onnx-fp32"` with the default per OS (`coreml` on macOS, `onnx-q8` on Linux); `cfg(not(any(macos, linux)))` keeps the error stubs. `PipelineConfig` gains an `embedder` stage (`StageCfg { name }`) so variants and Settings → AI → Pipeline name it (registry names `coreml`, `onnx-q8`, `onnx-fp32`, `recorded`, `fake`), and `pipeline_hash` changes with it. The service refuses to start with an `embedder` the binary was not built with (a clear message, not a silent fallback).

---

## 3. Sizing a Linux VM on the Mac mini M1 (16 GB)

Spike numbers (this Mac, Mac16,8, 12 cores; `ort` CPU EP; §8): q8 text graph, batch of 8 × 405-token chunks (Margin's `CHUNK_TARGET_TOKENS` is 384): **322 ms/chunk at 1 thread, 121 at 2, 104 at 4, 136 at 8** (it stops scaling at 4; the batch is small); fp32 104–110 ms/chunk at 4–8 threads (fp32 and q8 cost the same here because MLAS's fp32 GEMM on this core is fast and the q8 kernels gain little without `i8mm`; on the M1 the int8 `dotprod` path should win, unmeasured). Short texts: 3 docs in 26–97 ms. Session load 0.08 s (q8) / 0.2 s (fp32) from a warm page cache. Peak RSS **1.6 GB (q8)** and **1.96 GB (fp32)** for the text graph alone (ORT's arena plus the mmapped external data; the arena can be capped with `with_memory_limit`-style session config, to try in the area).

Expect an M1 vCPU at roughly half this core's throughput and the VM to take 4 of the 8 cores: budget **~250 ms per chunk at 3 threads**, i.e. ~14 000 chunks/hour, a 300-page textbook (≈ 1 200 chunks) in ~5 minutes, which is well inside "sources are ingested once". Queries are single short texts: ~30–60 ms, under the voice budget's 250 ms tool call (live-context plan §6.3) only with the retrieval cache warm; measure. Images: the vision encoder q8 (195 MB) is ~0.6 GB more RSS when loaded; audio encoder q8 (340 MB) ~1 GB more; Parakeet ~1.5 GB while transcribing. They load on demand and unload after idle, so the steady state is the text graph.

| Guest | vCPU | RAM | Disk | Notes |
|---|---|---|---|---|
| `margin-prod` (Linux) | 4 | 8 GB | 160 GB | service ~0.5 GB + ORT text 1.6 GB + encoders/Parakeet transient ≤ 2.5 GB + SurrealDB HNSW (100 k passages ≈ 300 MB vectors + graph) + page cache. `MemoryMax=7G` on the unit. |
| `margin-dev` (Linux) | 2–4 | 6 GB | 80 GB | same software, smaller corpus |
| `margin-ci` (Linux runner) | 4 | 4 GB | 80 GB | `cargo test --workspace` + release builds; can be suspended when idle |
| host | 2 cores, ≥ 2 GB | | | Tart, Orchard, the macOS runner only if still needed |

Three Linux guests on 8 cores oversubscribe CPU, not memory (8 + 6 + 4 + host ≈ 20 GB > 16 GB): **run prod and dev together, and CI only while dev is suspended**, or give dev 4 GB (the data is small). Lavapipe rendering wants every core it can get: the video queue stays serial, and `ORT` intra-threads = `vCPU − 1`. No swap in the guests; a failure should be loud.

---

## 4. Evals: the backend is a pipeline variant

- `evals/variants/linux-onnx-q8.toml` and `…-fp32.toml`: `baseline.toml` plus `[embedder] name = "onnx-q8"`. `baseline.toml` gains `[embedder] name = "coreml"` explicitly.
- `margin-eval record-embeddings --variant linux-onnx-q8` on the Mac mini **host** (the ONNX backend builds on macOS; the `onnx` feature) writes `evals/recorded/embeddings-embeddinggemma-2-onnx-q8+title-prefix-v1.bin` beside today's Core ML recording. Both recordings are over the same chunks (same chunker), so `margin-eval compare baseline linux-onnx-q8 --allow-embedder-change` reports recall@5, recall@10, MRR, nDCG@10 per query kind (`lexical`, `semantic`, `cross-modal`, `negative`) with the paired bootstrap interval. The dense leg's contribution is isolated by also comparing `dense.toml` with each embedder.
- `margin-eval compare-vectors A B` (new, small): mean/min/p5 cosine between the two recordings per modality (text, picture, sound), and the share of queries whose top-1 passage differs. This is the honest cross-backend number; it is reported, never used to allow mixing.
- Readers: the retrieval suite re-ingests `evals/corpus` through the service build under test, so the PDF/OCR/ffmpeg path is exercised by the Linux run (pdfium + tesseract text vs PDFKit + Vision text changes the chunks; the report lists documents whose chunk hashes differ and the OCR character-error rate on the scanned corpus page against its known text, a new `evals/golden/ocr.jsonl` with the two scanned pages' ground truth).
- **Gate to switch** (§4 of the deploy plan's promote step): on `evals/corpus`, the Linux variant within 1.0 point recall@5 and nDCG@10 overall and 3.0 per kind of the Core ML baseline (the same thresholds as `gates.toml`), negatives' false-positive rate ≤ baseline + 2, OCR CER ≤ 2× the Vision CER on the scanned pages, transcription WER ≤ baseline + 3 points on the corpus clip; latency reported, not gated. If cross-modal misses the gate, the picture path (vision encoder preprocessing) is suspect before the model is: re-check the resize rule first.
- CI stays on fakes: `RecordedEmbedder` with whichever recording the baseline names; a Linux CI job runs the whole workspace on the runner VM (the fake tests don't need the model). The real-tool reader tests (`crates/embed/tests/corpus.rs` extended with pdfium/tesseract/ffmpeg cases) run only with `MARGIN_REAL_TOOLS=1`, which the Linux runner sets because it has the apt packages; macOS runs skip them.

---

## 5. Deploy changes (amends `docs/plan-deploy.md`)

- **Guests**: `ghcr.io/cirruslabs/ubuntu:24.04` (Tart's Linux base image, cloud-init, `admin` with SSH; the licence question in the deploy runbook step 3 is the same). Created through Cluster Bar/Orchard like the macOS ones (`tart clone … && tart set … && tart run --no-graphics`). No two-guest limit: `margin-dev`, `margin-prod`, `margin-ci`; the macOS guests are deleted once prod runs on Linux (the Mac stays the owner's dev box, Q16's recommendation).
- **Provisioning** (`deploy/scripts/provision-linux.sh`): `apt install libc++1 tesseract-ocr tesseract-ocr-eng ffmpeg libheif1 bubblewrap python3 curl`; `uv` + `uv tool install --python 3.14 manimgx` and `margin-tts`; pdfium tgz → `/usr/local/lib/libpdfium.so`; `cloudflared` from Cloudflare's apt repo; user `margin`, `/etc/margin`, `/var/lib/margin` (0700); the systemd units; `sysctl`/AppArmor for user namespaces. The embedding and Parakeet models are downloaded by the service from Settings as today, or by `margin-service download-model` (new sub-command, headless) in the script.
- **Networking, secrets, backups**: unchanged in shape: one Cloudflare Tunnel per guest, Access on dev, two OAuth apps, backups before every deploy with `backup-margin.sh` getting a `systemctl stop/start` branch; restores and the prod → dev rules as written. The Keychain "Always Allow" prompts go away; `codesign` goes away.
- **CI**: build and test **inside the Linux runner VM** (`margin-ci`, an Orchard worker registered as a GitHub self-hosted runner with labels `self-hosted, linux, arm64, margin`), not cross-compiled from the macOS host: cross would need a GCC/Clang aarch64 sysroot, `libc++`, pdfium and ORT libraries for the target and would still not run the tests. `test.yml` becomes a matrix (macOS host runner: the app's iOS check and `dx` builds; Linux runner: `cargo test --workspace`, clippy, `MARGIN_REAL_TOOLS=1` reader tests). `build-service.yml` produces `margin-service-linux-aarch64-<sha>` on the Linux runner; `deploy-dev.yml`/`promote-prod.yml` copy it over SSH and `systemctl restart margin-service`, then check `/health` and `/api/version`. GitHub-hosted `ubuntu-24.04-arm` runners are free for public repos only; if the repo stays private, the self-hosted VM it is (Q9).
- **Media VM for LiveKit** (live-context Q10): the same Linux guest pattern; a fourth small guest later.

---

## 6. Build areas (exclusive files) and order

**First commit (orchestrator, on the build branch):** `margin_embed::Backend` + `embedder_version(backend)`, `ModelFiles.backend`, `download::Manifest` per backend (ONNX file list), `OnnxOptions`/`OnnxDtype`, cfg stubs for `OnnxEmbedder::load` (`unimplemented!`), the `onnx`/`linux-readers`/`heic`/`speech-onnx` features and the Linux dependency table; `PipelineConfig.embedder` (shared, `#[serde(default = "coreml")]`), `SandboxMode::LinuxBubblewrap`, `config.{embedder, secrets}` keys and per-OS paths, `Document.extractor` and the `embedder = $embedder` conjunct in `retrieval.rs` (one line each, shared files, in this commit), `ToolStatus` names for `tesseract`/`ffmpeg`/`bwrap`/`pdfium`/`margin-tts`; `docs/HANDOFF.md` row.

| Area | Worktree | Builds | Exclusive files |
|---|---|---|---|
| **LX-embed-text** | `agent/lx-embed-text` | `OnnxEmbedder` text path: session setup (CPU EP, PreferCPU, threads), tokeniser + prompts, padded batching, `embed_pipelined`, `count_tokens`, download manifest, unload-on-idle scaffold; dumps the encoder graphs' input names into `onnx/mod.rs`; tests on fakes (`Session` behind a tiny trait with a stub) and `real_model_tests()` on `MARGIN_REAL_MODEL=1` | `crates/embed/src/onnx/{mod,text,session}.rs`, `crates/embed/src/download.rs`, `crates/embed/tests/onnx_text.rs` |
| **LX-embed-media** (after text) | `agent/lx-embed-media` | vision/audio encoders + feature injection, the mel front end with the recorded-feature fixture, pdfium reader, tesseract OCR, `image`/libheif decode, ffmpeg sound/frames, `extract.rs` Linux arms, `pdf_page_picture`/`jpeg_for_model` Linux impls; validation notes vs the HF processor | `crates/embed/src/onnx/{vision,audio,mel}.rs`, `crates/embed/src/{pdf_pdfium,ocr_tesseract,image_linux,av_ffmpeg,extract}.rs`, `crates/embed/tests/{corpus.rs,fixtures/**}` |
| **LX-speech** | `agent/lx-speech` | `parakeet-rs` transcriber (spike first; `whisper-rs` fallback), model download, `Transcript` with word times; `margin-tts` script + `pocket_tts` dispatch | `crates/embed/src/transcript_parakeet.rs`, `crates/video/src/{lib.rs (voice fn only), margin_tts.py}` |
| **LX-service** | `agent/lx-service` | `FileStore`, store selection, `EmbedTools::load` by config, `download-model` sub-command, Settings/Tools rows, user-facing strings, `evals` live commands on both backends | `crates/service/src/{secrets,engines,config,main,video}.rs`, `crates/video/src/tools.rs`, `crates/evals/src/live.rs`, `crates/evals/src/bin/margin-eval.rs` |
| **LX-sandbox** | `agent/lx-sandbox` | bwrap profile and args, AppArmor/userns notes, the lavapipe render measurement on the fixture scene, `RenderError` texts | `crates/video/src/{sandbox_linux.rs,sandbox.rs}`, `crates/video/tests/*` |
| **LX-evals** | `agent/lx-evals` | `embedder` stage in variants, per-version recordings, `compare-vectors`, OCR/WER golden rows, report columns, the switch gate | `crates/evals/src/{recorded,metrics,report,cli}.rs`, `evals/variants/*`, `evals/golden/ocr.jsonl`, `evals/gates.toml` |
| **LX-deploy** | `agent/lx-deploy` | units, provisioning, workflows matrix, runbook amendments | `deploy/systemd/*`, `deploy/scripts/*`, `.github/workflows/*`, `docs/plan-deploy.md`, `docs/service-setup.md` |

**Order.** First commit → LX-embed-text, LX-service, LX-sandbox, LX-deploy in parallel → LX-embed-media and LX-speech (both need the session plumbing from embed-text) with LX-evals → integrate; the integrator runs the workspace on the Linux runner VM and the Mac → owner runs `record-embeddings` for both backends on the Mac mini host and `compare` (§4) → if the gate passes, prod moves (deploy runbook step 12, now a Linux guest, with the one-pass re-embed at first start) and the macOS guests are retired. Rule from the deploy plan stands: tests on fakes, no real model in CI, the eval runner by hand.

---

## 7. Risks, said plainly

1. **Licence wording.** The model's header says Apache-2.0 but links a Gemma licence and the Prohibited Use Policy; the same applies to the Core ML conversion in use today. Not a blocker for the switch, but "sell this" needs the owner to read it once (Q1).
2. **`ort` is a release candidate** (rc.14, 2026-10-12; no 2.0 stable): APIs moved between rc.13 and rc.14 (the spike hit `Session::builder(&env)`, `inputs()`, ndarray 0.17, the auto-device default). Pin exactly, as every other dependency is pinned.
3. **Media preprocessing fidelity.** The image resize rule and the audio mel front end are reimplemented; a wrong constant gives plausible but worse vectors. The fixture-based validation (§2.1) and the cross-modal eval gate exist for this.
4. **OCR quality drops** on photos and handwriting (tesseract vs Vision). The eval reports it; if it matters, `ocrs` or a small ONNX OCR model behind the same `extract` seam is a later variant.
5. **Lavapipe rendering speed** for manimgx is unmeasured; a 2-minute lesson video could take many minutes on 4 vCPUs. Measured first in LX-sandbox; if too slow, videos keep rendering on the Mac (a `render_host` config) until a GPU-capable box exists.
6. **`parakeet-rs` is young** (0.4.x this week). The area starts with a 1-hour spike; `whisper-rs` is the named fallback.
7. **User namespaces** on Ubuntu 24.04 need an AppArmor profile or a sysctl for `bwrap`; a guest where neither is set renders nothing (loudly, `RenderError::Sandbox`).
8. **Memory.** ORT's arena made q8 cost 1.6 GB RSS for a 314 MB model; on an 8 GB guest with encoders and Parakeet loaded at once that is tight. Idle unloading and `MemoryMax` are the guards; measure on the M1 before fixing the sizes.

---

## 8. Spike (done on this Mac, 2026-10-14; nothing committed)

A throwaway crate in the session scratchpad (not in the repo): `ort = "=2.0.0-rc.14"` (`ndarray` feature), `ndarray 0.17`, `tokenizers 0.23.2` (`onig`). Downloaded from `onnx-community/embeddinggemma-2-ONNX`: `tokenizer.json`, `config.json`, `onnx/model_quantized.onnx(+_data, 314 MB)`, `onnx/model.onnx(+_data, 1 084 MB)`; deleted afterwards. The program loads the text graph on the CPU EP, prints the graph's inputs/outputs (§2.1), embeds three `title: none | text: …` sentences and two `task: search result | query: …` queries (empty `[0, 512]` tensors for `image_features`/`video_features`/`audio_features`), prints the dimension, a norm, cosines and a timing for 8 × 405-token chunks.

Essentials of the code (rc.14 API):

```rust
let env = ort::init().build()?;
let mut session = Session::builder(&env)?
    .with_intra_threads(threads)?
    .with_auto_device(AutoDevicePolicy::PreferCPU)?          // without this, macOS picked CoreML and failed
    .with_execution_providers([ort::ep::CPU::default().build()])?
    .commit_from_file(format!("{dir}/onnx/{variant}.onnx"))?;
let none = Array2::<f32>::zeros((0, 512));
let out = session.run(ort::inputs! {
    "input_ids" => TensorRef::from_array_view(&ids)?,          // i64 [batch, seq], right-padded
    "attention_mask" => TensorRef::from_array_view(&mask)?,
    "image_features" => TensorRef::from_array_view(&none)?,
    "video_features" => TensorRef::from_array_view(&none)?,
    "audio_features" => TensorRef::from_array_view(&none)?,
})?;
let v = out.iter().find(|(n, _)| *n == "sentence_embedding").unwrap().1.try_extract_array::<f32>()?; // [batch, 768]
```

Result (q8, 4 threads): `dim = 768`, norm 1.0000; cos(query "How do plants make food from sunlight?", photosynthesis doc) = **0.784** vs 0.594 / 0.515 for the Fourier and TCP docs; cos(query "decomposing a periodic signal into sinusoids", Fourier doc) = **0.791** vs 0.596 / 0.601; cos(Fourier doc, photosynthesis doc) = 0.627. fp32 gives the same to three decimals (0.784 / 0.792); **q8 vs fp32 cosine per vector 0.9999–1.0000**. Timings and RSS are in §3. The first two runs failed before the auto-device fix: `CoreMLExecutionProvider … the ort_value must contain a constructed tensor` and `Input (video_features) has a dynamic shape ({-1,512}) but the runtime shape ({0,512}) has zero elements. This is not supported by the CoreML EP`. Not done in the spike: the vision and audio encoders, the Core ML model side by side (so the cross-backend cosine in §2.1 is inferred from the two cards, not measured), and a run on an M1 or on Linux.

---

## 9. Owner questions, with recommendations

1. **Licence.** The EmbeddingGemma 2 card says Apache-2.0 but links the Gemma licence and the Prohibited Use Policy; fine for Margin's use as far as I can read it, but for selling, read it once. Recommend: proceed; note it in the product's third-party list.
2. **Precision.** q8 for all three graphs (0.85 GB, cosine 0.9999 to fp32 on text in the spike), or fp32 text (1.1 GB, same speed on this Mac, likely slower on the M1)? Recommend q8; fp32 stays a variant for the eval.
3. **Re-embed on switch.** Accepted in principle; confirm the shape: one pass at first start of the Linux prod, dense retrieval limited to the new vectors while it runs, BM25 untouched. Recommend yes; the alternative (keeping Core ML vectors until each document changes) mixes vector spaces.
4. **OCR engine.** tesseract from apt (better on dense pages, a system package) or `ocrs` (pure Rust, no packages, weaker)? Recommend tesseract; the eval decides whether photos need more.
5. **Secrets on Linux.** Plain 0600 files under `/var/lib/margin/secrets` (recommended, simple, backed up with the data dir — or excluded: say which), or systemd encrypted credentials (host-bound, not restorable to another box without the key)? Recommend files, excluded from backups by default.
6. **Sandbox.** bubblewrap with user namespaces enabled on the guest (recommended), or no scene rendering on Linux until a GPU box exists? Recommend bwrap; the lavapipe measurement decides where videos render.
7. **Narration voice on Linux.** pocket-tts (same voices as the Mac; its licence isn't stated on PyPI, needs reading) or Piper (MIT, different voices)? Recommend pocket-tts if the licence allows, else Piper.
8. **Keep the macOS backends buildable** (cfg-selected, `coreml` default on macOS) so the owner's Mac stays a full dev/eval box? Recommend yes; it also lets the A/B eval run on one machine.
9. **CI placement.** A Linux runner VM on the Mac mini (self-hosted, recommended; private repo) or GitHub-hosted `ubuntu-24.04-arm` (free only for public repos)?
10. **VM sizes.** prod 4 vCPU / 8 GB / 160 GB, dev 4 / 4–6 GB / 80 GB, CI 4 / 4 GB, CI suspended while dev runs; confirm the Mac mini's exact cores/memory (deploy plan Q5 is still open).
11. **Order.** Build this before L4 (meetings) as live-context Q16 recommends, with the eval gate deciding the prod move? Recommend yes; it is the first infrastructure phase and unblocks LiveKit on a Linux guest too.
