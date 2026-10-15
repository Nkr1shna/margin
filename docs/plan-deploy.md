# Deploy plan: dev and prod on the Mac mini

Written 2026-10-14 on `claude/agent-build` (`af8cbe6`), updated the same day with the facts of the Mac mini setup and built as area E-deploy (`agent/e-deploy`). Read: `docs/service-setup.md`, `PLAN.md`, `docs/plan-dogfood.md` §5, `docs/plan-live-context.md` §5.2 and Q10/Q16, `crates/service/src/{config,version,restore,main}.rs`, `deploy/`, `.github/workflows/`, `evals/README.md`. Linux service guests are planned separately (`docs/plan-linux-service.md`); the scripts here detect the guest's OS and stop on anything but macOS (launchd) rather than guessing at systemd.

**Cluster Bar / Orchard.** The VMs are macOS 26 Tart guests that Orchard schedules on the Mac mini, driven from Cluster Bar. Orchard's worker names each guest's disk `orchard-<name>-<uid>-<restart count>` and runs `tart clone <image>` for every new name (`internal/worker/vmmanager/tart/tart.go`, `cloneAndConfigure`; `internal/worker/ondiskname`). So **every restart under "restart on failure", every reschedule and every recreate starts from a fresh clone of the base image: the guest's own disk is thrown away.** §5 is built around that.

## 1. Environments

| | dev | prod |
|---|---|---|
| Host | Mac mini, M1, 16 GB | same |
| Guest | macOS 26 Tart VM `margin-dev` (Orchard, Cluster Bar) | macOS 26 Tart VM `margin-prod` |
| Size | 4 CPU, **4 GB**, 60 GB disk | 4 CPU, 8 GB, 100 GB disk |
| Reached as | `admin@margin-dev.local` (DHCP address changes; the mDNS name doesn't), key `~/.ssh/margin_deploy` on the host | `admin@margin-prod.local`, same key |
| Public URL | `https://devmargin.krishnanelloore.com`, tunnel `margin-dev` | `https://margin.krishnanelloore.com`, tunnel `margin-prod` |
| Runs | `margin-service serve` (LaunchAgent), `cloudflared` (LaunchAgent), `manimgx`, `fluidaudiocli` | same |
| Persistent | host `~/MarginVMs/margin-dev` → guest `/Volumes/My Shared Files/margin` | host `~/MarginVMs/margin-prod` → same guest path |
| Later | LiveKit at `livedevmargin.krishnanelloore.com` | `livemargin.krishnanelloore.com` |

**Why macOS guests.** `crates/embed` is macOS-only (`objc2-core-ml`, `objc2-pdf-kit`, `objc2-vision`, `objc2-av-foundation`), secrets are the login Keychain through `keyring`, scene code renders under `sandbox-exec`, and the service is a LaunchAgent that needs a logged-in GUI session. Tart's base images auto-login `admin` with SSH on.

**Two-VM limit.** Apple's Virtualization framework runs at most two macOS guests per host; dev and prod take both, so the CI runner lives on the host (§4).

**Memory.** The M1 has 16 GB: 4 for dev, 8 for prod, 4 for macOS, the runner and builds. Dev's 4 GB must hold the embedding model (EmbeddingGemma, ~1.4 GB loaded), SurrealDB's caches, the service and macOS itself. To make that visible: `GET /api/version` reports `rss_bytes` (the service's resident memory) and `physical_memory_bytes` (the VM's), `deploy.sh` prints the RSS after each deploy, and the service logs a warning at start when the machine has less than 6 GB (`version::LOW_MEMORY_BYTES`), which dev always does. If dev swaps under an ingest (watch `rss_bytes` and Activity Monitor in the VM window), the fixes are: dev to 6 GB (prod to 6 GB too, leaving 4 for the host), or not loading the embedder in dev until first use.

**No Neural Engine in a VM.** Core ML inside a guest has GPU and CPU only. Measure before trusting dev numbers: ingest the eval corpus (`evals/corpus`, ~5 MB) on the host Mac and in each VM, then compare the `embed` trace rows (`margin-eval export --service-url …`). Record host and VM medians in `evals/reports/`. If the VM is more than ~3× slower, prod embedding stays acceptable (sources are ingested once) but the eval runner (§6) should run on the host with the VM only as the trace source.

## 2. Networking

- One Cloudflare Tunnel per VM, `cloudflared` as a LaunchAgent in the guest, forwarding to `127.0.0.1:8787`. No ports open on the Mac mini; the VMs use Orchard's default NAT network. TLS terminates at Cloudflare.
- **Single-level subdomains** (`devmargin.`, `margin.`, later `livemargin.`, `livedevmargin.`) so Cloudflare's free universal certificate covers them; `dev.margin.` would need an Advanced Certificate. `public_url` in each guest's `service.toml` is its hostname. The prod hostname used to route to the tunnel on the owner's Mac (`docs/service-setup.md`); `cloudflared tunnel route dns -f margin-prod margin.krishnanelloore.com` moves it, and the Mac's own tunnel then serves nothing until Q9.
- Put **Cloudflare Access** (GitHub identity, the owner's login only) in front of dev so the dev service is never reachable by strangers; prod stays as the service's own OAuth allow-list. With Access on, `/api/version` on the dev hostname needs the Access cookie; `deploy.sh` checks it inside the guest instead.
- **GitHub OAuth apps**: one per environment ("Margin" and "Margin dev"), because an OAuth app has one callback URL (`https://<host>/auth/github/callback`).
- Cloudflare refuses bodies over 100 MB; multipart uploads already handle it (PLAN.md Upload limits).

## 3. Secrets

Each VM has its own login Keychain; the service name stays `com.krishnanelloore.margin`. Items per VM: `github-client-secret`, `openai-api-key` (and the providers brief's Claude key), `media-signing-key` (generated). The owner enters them **in the VM's own window** (Screen Sharing or the Cluster Bar window), never over SSH (the login Keychain is locked there) and never in the repo, GitHub Actions secrets, Cluster Bar or a device: `margin-service setup` for the GitHub secret, Settings → AI provider in the app for model keys. CI signs the binary with a stable identity (§4) so the Keychain ACL keeps matching across deploys.

**The Keychain is on the guest's disk, so a recreated VM has an empty one** (§5). After a recreation the owner runs `margin-service setup` again in the VM's window (Return keeps every value in `service.toml`; it asks only for the secret), enters the model keys in Settings again, and clicks Always Allow once. `media-signing-key` is made again at start, so media links handed out before die; nothing else is lost. `provision-guest.sh` says when the secret is missing. Moving secrets to a Keychain file on the share would survive recreation but put them on the host's disk in a file the runner user can read; not done (owner question 12).

GitHub Actions holds **no secrets**: deploys go runner → VM over the host's NAT network with the host's key, and every workflow uses only `github.token` (read-only, `actions: read` for artifacts). No workflow prints a secret.

## 4. CI/CD: GitHub Actions on the host's runner

**Runner.** `~/actions-runners/margin` on the Mac mini, name `mac-mini-host`, labels `self-hosted, macOS, ARM64, margin`; Rust 1.99, `dx` 0.7.10, Xcode 27, `gh`. A second runner on the host serves another repository with the label `clusterbar`, so **every job targets `[self-hosted, macOS, ARM64, margin]`**. The runner user holds `~/.ssh/margin_deploy`, the one deploy key for both VMs, and the self-signed code-signing identity "Margin Service Signing" (owner, runbook step 8). **Linux runner:** `margin-ci`, an Orchard VM (4 CPU / 6 GB / 80 GB plus 4 GB swap, `--os linux`) from the image `margin-linux-v1` built by `deploy/image/margin-linux.pkr.hcl` (Packer + Tart, Ubuntu 24.04 with Rust, clang/libc++, libheif, tesseract, ffmpeg, bubblewrap, pdfium, uv + manimgx, cloudflared, gh and the runner). Its startup script (`deploy/ci/margin-ci-startup.sh`) mounts the host share `~/MarginVMs/margin-ci`, where the runner's files and registration live (so a re-clone keeps them), and starts it under systemd; work folder and Cargo cache stay on the VM disk. Registered once with `deploy/ci/register-margin-ci.sh`.

| Workflow | Trigger | Does |
|---|---|---|
| `ci.yml` (**CI**) | pull requests from this repository (CI never runs on push), `workflow_dispatch` | `linux` on **margin-ci** (`[self-hosted, Linux, ARM64, margin]`): `cargo fmt --check`, clippy and `cargo test` for the workspace minus `margin-app`; `macos` on the Mac host: `margin-app` clippy, tests and the iOS simulator check, plus `cargo check -p margin-service` (the macOS backends still build); both are required checks (**CI / Linux**, **CI / macOS**) in the repository ruleset |
| `cancel-closed-pr-ci.yml` (**CI cleanup**) | a PR merged or closed | cancels that PR's unfinished CI runs (on a GitHub-hosted runner, so it never waits behind them); a new push to an open PR already cancels the previous run through `ci.yml`'s concurrency |
| `image.yml` | a merge into `dev` that changes `deploy/image/**` or `rust-toolchain.toml`, `workflow_dispatch` | on the Mac host: `packer build deploy/image/margin-linux.pkr.hcl` (Ubuntu 24.04 + the pinned Rust) and `tart push` to **`ghcr.io/nkr1shna/margin-linux`** (`<date>-<sha>` and `latest`) with the job's `GITHUB_TOKEN`; the image holds no secrets |
| `build-service.yml` | a merge into `dev` or `release` that changes the service (paths: `crates/{service,agent,ai,embed,video,shared}`, `Cargo.toml`/`Cargo.lock`, `deploy/`, its workflows), `workflow_dispatch` | no test job (the PR's CI checks already passed on a branch kept up to date with the base); on **margin-ci**: `cargo build --release --locked -p margin-service` (lld, Cargo cache named after the environment) with `MARGIN_BUILD_SHA`; checks it is a Linux aarch64 binary stamped with that commit; uploads `margin-service-<sha>`. Linux binaries aren't code-signed. |
| `deploy-dev.yml` | after a successful `build-service` on `dev` (push or dispatch, this repository), or `workflow_dispatch` with a build run id | `MARGIN_DEPLOY_ONLY=dev`; `deploy.sh dev`: preflight, copy the binary, stop, backup (§5), swap, start, wait for `/health`, check `/api/version` reports `<sha>` and `dev`, roll back to the previous binary if not. Uploads the installed bytes again as `deployed-dev-<sha>`. GitHub Environment `dev` (no reviewer; a deployment record) |
| `deploy-prod.yml` | after a successful `build-service` on `release` (push or dispatch, this repository), or `workflow_dispatch` with a build run id of `release` | Environment **`prod`** (admits only the `release` branch); `MARGIN_DEPLOY_ONLY=prod`; `deploy.sh prod` with that build's artifact; uploads `deployed-prod-<sha>`. Shipping a change = merging `dev` into `release` (replaces promote-prod). |
| `apps.yml` | a merge into `dev` or `release` that changes the app (paths: `crates/{app,shared}`, `Cargo.toml`/`Cargo.lock`, `apps.yml`), tag `v*`, or `workflow_dispatch` (prod, dev or both; artifacts only) | per environment, `MARGIN_DEFAULT_SERVICE_URL` set: `dx bundle -p margin-app --platform desktop --package-types macos --release` → ad hoc signed `.dmg`; `dx build -p margin-app --platform android --release` → APK (skipped while the runner has no Android SDK). Published as GitHub releases: rolling pre-release `dev-latest` after each dev deploy, `prod-<sha>` (latest) after each promotion, the tag's name for `v*`. iOS TestFlight waits on the Apple Developer account (Q7). |

Cargo builds keep their output in `~/.cache/margin-ci/target-{test,release}` on the runner (outside the checkout, which `actions/checkout` wipes), so CI and CD rebuild only what changed. Prune it if the disk fills (`rm -rf ~/.cache/margin-ci`; the next run rebuilds once).

**Check Linux CI locally before pushing.** `deploy/local/linux-check.sh` (any Apple-silicon Mac with Tart) clones `ghcr.io/nkr1shna/margin-linux:latest` into a throwaway VM, shares the checkout in read-only and runs `deploy/ci/linux-ci.sh` — the same script CI / Linux runs — then deletes the VM; `--release` also builds `margin-service`. Build output and crates are cached in `~/.cache/margin-linux-check`. The package must be public for an anonymous `tart pull` (GitHub → the package → Package settings → Change visibility), else `tart login ghcr.io` with a token that can read packages.

**Keeping the deploy key and prod safe.** `deploy.sh` and `provision-guest.sh` refuse any environment but `MARGIN_DEPLOY_ONLY` when it is set, so a deploy-dev job never opens a connection to margin-prod. Prod is reachable only from the `prod` Environment's job, which waits for the owner's approval. What protects the scripts themselves is that only the owner's commits reach the deploy branch; the owner applies these settings **[owner, GitHub]**:

1. Settings → Actions → General → "Fork pull request workflows from outside collaborators": **Require approval for all outside collaborators**; and keep the repository private. `ci.yml` also skips pull requests whose head is a fork, so fork code never runs on the self-hosted runner.
2. Branches: **`dev`** (the default branch; every push builds and deploys dev) and **`release`** (every push builds and deploys prod; merge `dev` into `release` to ship). Branch protection and required reviewers need a paid plan on a private repository, so the guard is the Environments below plus the owner being the only one who can push.
3. Settings → Environments → `prod`: deployment branches = **Selected branches: `release`**, so only a build of `release` can reach prod (deploy-prod.yml also checks the build run's branch).
4. Settings → Environments → `dev`: deployment branches = `dev`.
5. Settings → Actions → General → Workflow permissions: **Read repository contents** (the default token stays read-only).
6. Settings → Actions → Runners: the `mac-mini-host` runner in the default group, used only by this repository.

The apps pick the environment on the sign-in screen: **Prod / Dev / Custom** (`config::PRESETS`, `ui/signin.rs`). The device token is stored per URL (`app/secrets.rs`), so one desktop app switches between dev and prod without mixing tokens.

## 5. Data: what persists, what is wiped

**Deploys never wipe the database.** A deploy replaces one file, `bin/margin-service`; `data/`, `backups/` and `service.toml` are never written by it except for the backup copy it takes first.

**Orchard recreation would wipe it**, if it were on the guest's disk (see the top). So everything that must survive lives in one host folder per VM, shared into the guest with Orchard `hostDirs` (Tart `--dir`):

```
orchard create vm margin-dev  --image <macOS 26 base> --cpu 4 --memory 4096 --disk-size 60 \
  --restart-policy OnFailure --host-dirs margin:/Users/<owner>/MarginVMs/margin-dev \
  --startup-script @<(deploy/scripts/provision-guest.sh --startup-script dev)
orchard create vm margin-prod --image <macOS 26 base> --cpu 4 --memory 8192 --disk-size 100 \
  --restart-policy OnFailure --host-dirs margin:/Users/<owner>/MarginVMs/margin-prod \
  --startup-script @<(deploy/scripts/provision-guest.sh --startup-script prod)
```

(or the same fields in Cluster Bar). macOS guests automount the share at `/Volumes/My Shared Files/margin`.

| Where | What | Survives a deploy | Survives Orchard recreating the VM |
|---|---|---|---|
| Share (`~/MarginVMs/margin-<env>` on the Mac mini) | `.margin-persistent` (marker), `service.toml`, `data/` (`db/`, `sources/`, `uploads/`, `videos/`, `models/`), `backups/`, `bin/` (the installed binary, the previous one, the scripts), `cloudflared/` (`cert.pem`, tunnel credentials, `config.yml`; `~/.cloudflared` links here) | yes (only `bin/margin-service` changes) | yes |
| Guest disk | macOS, Homebrew, `cloudflared`, `manimgx`, `fluidaudiocli`, the LaunchAgent plists, the login Keychain (secrets), `~/Library/Logs/Margin`, the guest's SSH host key | yes | **no**: `provision-guest.sh <env> --rekey` puts back everything but the Keychain secrets and the tools' installs (runbook 4); the owner re-enters secrets (§3) |
| Orchard startup script | hostname `margin-<env>` (so `.local` resolves), the deploy public key in `authorized_keys` | — | re-applied at every start |

**Guards.** `deploy.sh` refuses to deploy unless, inside the guest: the share is mounted at `/Volumes/My Shared Files/margin`, it is another file system than the guest's root disk, its marker names this VM, `service.toml` on it says this environment, and `data_dir` is on the share. The service's LaunchAgent waits for the marker before it starts the binary (which itself lives on the share), so a guest that boots before the share mounts never creates a fresh database on its own disk.

**Backups** live on the share, which is the Mac mini's disk, so they survive the VM too:
- `deploy.sh` runs `backup-margin.sh --label before-<new sha>-from-<old sha>` after stopping the service and before swapping the binary. It refuses while the service runs, writes `backups/<date-time>-<label>/data`, and keeps the newest 10.
- The `com.krishnanelloore.margin.backup` LaunchAgent runs `backup-margin.sh --stop-service --label nightly` at 03:30.
- `deploy/scripts/backup-margin.sh` is the one backup script: the same file `docs/plan-dogfood.md` §5 asks for (it moved from `scripts/`), and on the owner's Mac it runs as before with no arguments.
- Off the Mac mini: `~/MarginVMs/margin-prod/backups` copied weekly to an external disk **[owner: which disk]** (runbook 13).

**Migrations** run at service start (`db.rs`: `DEFINE … IF NOT EXISTS`, the one-pass `passages-v1` re-embed). A deploy is: backup → new binary → start → `/health` → `/api/version`. A build that doesn't come up is rolled back to `bin/margin-service.prev` automatically; data is restored by hand from the printed backup when the failed build had migrated it.

**SurrealKV on a shared folder.** The database now sits on virtio-fs, not the guest's APFS disk. The restore drill (runbook 12) and a hard stop (`orchard` restart of dev under an ingest, then `/health` and a search) must pass before prod holds the owner's library; if SurrealKV misbehaves on virtio-fs, the alternative is the database on the guest disk with `backup-margin.sh` every hour to the share, accepting up to an hour's loss on recreation.

**Prod → dev restore** (`restore-into-dev.sh`, by hand only, refuses CI and needs a terminal): copies the newest prod backup on the host from `~/MarginVMs/margin-prod/backups` (no SSH to prod at all), asks the owner to type `restore`, stops dev, backs dev up (`pre-restore`), puts the copy in dev's `data/` (dev's old `data/` kept beside it as `data.before-restore-<time>`), runs `margin-service scrub-restored` (deletes every `device` row so prod devices can't sign in to dev, and every trace row and kept rollup so dev evals don't read prod usage; refuses a config whose environment isn't dev), and starts dev. Secrets aren't copied.

**Tests and evals never see prod**: CI (`ci.yml`) runs on fakes; `margin-eval` requires `--service-url` for `run`, `export` and `report --service`, refuses the host `margin.krishnanelloore.com` when parsing, and refuses any service whose `/api/version` says anything but `dev` (which also catches the owner's own Mac, whose config predates `environment` and so is `prod`); the runner reaches prod only from the approved `prod` job.

## 6. Traces and evals

The eval runner (`margin-eval run … --service-url https://devmargin.krishnanelloore.com`) reads cost, latency and cache numbers from **dev** traces over `GET /api/traces` with the owner's device token, and keeps its own `data_dir/evals/`. Dogfood harvesting (`margin-eval harvest`, dogfood plan §6) also points at dev and must take `--service-url` the same way (`Command::needs_service_url`, `cli::check_service`). With Cloudflare Access on dev, the runner uses the service in the VM directly (`http://margin-dev.local:8787` is not reachable: the service binds 127.0.0.1) or an Access service token; owner question 6 decides. The opt-in eval job is `workflow_dispatch` only and runs on the host runner so embedding uses the Neural Engine.

## 7. Runbook for a Claude Code agent on the Mac mini

Steps marked **[owner]** need the owner. Everything else is the agent at the host's shell, in a checkout of the deploy branch.

0. Done by the owner already: Cloudflare zone `krishnanelloore.com`; tunnels `margin-dev` and `margin-prod`; VMs `margin-dev` (4/4 GB/60 GB) and `margin-prod` (4/8 GB/100 GB) from the macOS 26 base image under Orchard with restart on failure; the runner `mac-mini-host`; `~/.ssh/margin_deploy`.
1. `deploy/scripts/provision-guest.sh dev` once on the host: it creates `~/MarginVMs/margin-dev` with its marker and stops, saying the share isn't mounted. **[owner, Cluster Bar]** Add the shared folder `margin:/Users/<owner>/MarginVMs/margin-dev` (Orchard `hostDirs`) and the startup script from `provision-guest.sh --startup-script dev` to `margin-dev`; Orchard restarts it (a fresh clone). Same for prod.
2. If a VM already holds something on its own disk (a tunnel login, a `service.toml`, a database), copy it out **before** the owner adds the share in step 1, because that restart is a fresh clone: stop the service in the guest, then from the host `scp -r -i ~/.ssh/margin_deploy 'admin@margin-dev.local:.cloudflared/*' ~/MarginVMs/margin-dev/cloudflared/` and `scp -r … 'admin@margin-dev.local:Library/Application Support/Margin/{service.toml,data}' ~/MarginVMs/margin-dev/`, then set `data_dir` in the copied `service.toml` to `/Volumes/My Shared Files/margin/data` and add `environment = "dev"`.
3. `provision-guest.sh dev --rekey` (the restarted VM has a new host key): installs `cloudflared`, links `~/.cloudflared` to the share, writes a starter `service.toml` (environment, public URL, `data_dir` on the share), installs both LaunchAgents and starts the backup one. It prints what is left.
4. In the guest over SSH: install `manimgx` and `fluidaudiocli` the way Settings → Tools shows (they live on the guest disk; repeat after a recreation). **[owner]** Change the guest's `admin` password from the image default and update it in Orchard's VM spec.
5. **[owner, in the VM's window]** `cloudflared tunnel login` if the share's `cloudflared/` has no `cert.pem`; the agent then runs the tunnel's `create` (or reuses the existing tunnel's credentials JSON copied into `cloudflared/`), `route dns -f margin-dev devmargin.krishnanelloore.com`, and `provision-guest.sh dev --tunnel-uuid <UUID>`, which writes `cloudflared/config.yml` from `deploy/cloudflared/config.yml` and installs the cloudflared LaunchAgent. **[owner]** Cloudflare Access policy on `devmargin.krishnanelloore.com`.
6. **[owner, GitHub]** The two OAuth apps (callbacks `https://devmargin.krishnanelloore.com/auth/github/callback`, `https://margin.krishnanelloore.com/auth/github/callback`).
7. **[owner, GitHub]** The settings in §4 (fork approval, branch rule, Environments `dev` and `prod`, read-only token).
8. Push the deploy branch; `build-service` runs `test` then builds; `deploy-dev` installs the binary on the share and starts it. The first start fails `validate_for_serving` (no client id yet): **[owner, in the dev window]** `MARGIN_CONFIG="/Volumes/My Shared Files/margin/service.toml" "/Volumes/My Shared Files/margin/bin/margin-service" setup` (environment `dev`, the "Margin dev" app), click Always Allow on the first Keychain prompt, then re-run the deploy-dev job. Check `margin-service` answers inside the guest: `curl http://127.0.0.1:8787/api/version`.
9. **[owner]** Open the desktop app, pick **Dev**, sign in with GitHub, enter the model keys in Settings, download the embedding model. Run the §1 embedding measurement and watch `rss_bytes`.
10. **[owner]** Merge `dev` into `release` and push; `build-service` and `deploy-prod` run; repeat steps 8–9 on prod (environment `prod`, the "Margin" app).
11. **Recreation drill**: restart `margin-dev` from Orchard (a fresh clone). Then `provision-guest.sh dev --rekey`, the step-4 tools, **[owner]** `margin-service setup` (secret only) and the model keys; the library, devices and backups are all still there.
12. Decide Q9 (prod data): if prod takes the owner's library, stop the service on his Mac, `deploy/scripts/backup-margin.sh`, copy the backup's `data/` to `~/MarginVMs/margin-prod/data` with prod stopped, start, point the app at **Prod**. Then a restore drill: `restore-into-dev.sh`.
13. Weekly copy of `~/MarginVMs/margin-prod/backups` to an external disk **[owner: which disk]** (a host LaunchAgent; `tart export` is no longer needed for data, since the VM holds none).

## 8. What changes in the repo (area E-deploy)

- `deploy/scripts/{common.sh,provision-guest.sh,deploy.sh,backup-margin.sh,restore-into-dev.sh}` (`backup-margin.sh` moved from `scripts/`; `common.sh` holds the hosts, the share's path and the persistence checks), `deploy/launchd/{com.krishnanelloore.margin.service,com.krishnanelloore.margin.backup}.plist` (templates with `__PERSIST__`, `__HOME__`), `deploy/cloudflared/config.yml` (template with `<HOST>`, `<UUID>`, `<CREDS_DIR>`), `.github/workflows/{test,build-service,deploy-dev,promote-prod,apps}.yml`.
- `crates/service/src/config.rs`: `environment` (`"dev" | "prod"`, default `"prod"`), validated, asked first by `setup`. `crates/service/src/version.rs`: `GET /api/version` → `{ version, sha (MARGIN_BUILD_SHA at build time), environment, started_at, rss_bytes, physical_memory_bytes }`, public like `/health`; the low-memory start-up warning. One route line each in `margin_shared::routes` (`VERSION`) and `server.rs`. `crates/service/src/restore.rs` and `margin-service scrub-restored`; `margin-service version`.
- `crates/app/src/config.rs`: `DEFAULT_SERVICE_URL` from `option_env!("MARGIN_DEFAULT_SERVICE_URL")` (prod otherwise), `PRESETS` (Prod, Dev) and `preset_for`; `ui/signin.rs`: the Prod / Dev / Custom radio group above the address (the field shows for Custom, remembering what was typed).
- `crates/evals`: `--service-url` required for `run`, `export` and `report --service`, refusing the prod host and any non-dev `/api/version`; `service_url` left `evals/runner.toml`.
- `docs/service-setup.md`: a note that the VMs follow this plan. `docs/HANDOFF.md`: an E row.
- No change to data layout inside `data_dir`, Keychain names, auth, or the wire contract beyond the one route.

## 9. Owner questions, with recommendations

Answered by the setup: 1 (domain: `krishnanelloore.com`, single-level subdomains), 4 (two VMs), 5 (sizes, §1).

2. **Which branch deploys to dev?** Recommend `claude/agent-build` now (every phase merges there); switch to `main` once `main` is caught up (still at `dbd08ff`). The workflows name the branch in `build-service.yml`, `deploy-dev.yml` and `test.yml`'s `branches-ignore`.
3. **What promotes to prod?** Built: the exact `deployed-dev-<sha>` artifact, by deploy-dev run id, approved in the `prod` Environment.
6. **Cloudflare Access on dev** (GitHub identity, owner only)? Recommend yes; the eval runner then needs an Access service token (stored in the runner user's Keychain, read by `margin-eval`, not built) or runs inside the dev VM.
7. **App builds.** Desktop `.dmg` and Android APK from CI now; iOS TestFlight needs the Apple Developer account: now or after phase 5? Recommend after phase 5.
8. **Signing identity.** None for the service: it is a Linux binary now (the repository secrets `MARGIN_SIGNING_P12_*` are no longer used and can be deleted). Developer ID later for the desktop app and TestFlight.
9. **Does prod hold the owner's real library now?** Recommend not yet: after two weeks of dev, the §1 measurement, the recreation drill (runbook 11) and the restore drill pass, move it (runbook 12) and retire the Mac's service.
10. **Prod → dev restores**: on demand only (built that way), or nightly?
11. **Who else may build?** Built for: only the owner pushes the deploy branch; forks never run on the runner (§4 settings).
12. **Secrets across recreation**: re-enter them after a recreation (now), or a Keychain file on the share (survives, but readable by the runner user on the host)? Recommend re-entering; recreations should be rare.
13. **Dev memory**: keep 4 GB and watch `rss_bytes`, or move dev to 6 GB (prod to 6 GB)? Recommend measuring during the first real ingest before changing.
