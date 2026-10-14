# Deploy plan: dev and prod on the Mac mini

Written 2026-10-14 on `claude/agent-build` (`af8cbe6`). Read: `docs/service-setup.md`, `PLAN.md`, `docs/plan-dogfood.md` §5, `docs/plan-live-context.md` §5.2 and Q10/Q16, `crates/service/src/{config,secrets,main,db,trace}.rs`, `crates/embed/Cargo.toml`, `crates/app/src/{config,secrets,state}.rs`, `evals/README.md`. Owner: dev and prod on the Mac mini, subdomains of a domain he owns, set up through **Cluster Bar** and GitHub Actions, with him at the machine for logins and approvals. Nothing here changes the owner's live instance on his own Mac; that stays as `docs/service-setup.md` describes until Q9 is answered.

**Cluster Bar.** It is on the Mac mini and I have not read it. The closest thing on this machine is `~/Developer/tart-ui` ("Tart Desk"): a local GUI over `tart` and Orchard that runs only allow-listed subcommands (`server/allow.mjs`: `clone set run ip stop suspend export import …`, `orchard create vm / worker …`), keeps per-VM run profiles in `~/.tart-desk/state.json`, and documents every `tart run` option in `web/help.js` (`--no-graphics`, `--net-bridged`, `--net-softnet-expose`, `--dir=name:path:ro`, `--suspendable`). It has no GitHub-runner feature. The plan assumes Cluster Bar does what the owner said: creates Tart VMs and GitHub Actions runners. Step 0 of the runbook checks that and falls back to the plain `tart` commands written beside each step. `tart` 2.32.1 and `orchard` are installed here; `cloudflared` is not.

## 1. Environments

| | dev | prod |
|---|---|---|
| Guest | macOS VM `margin-dev` (Tart, `ghcr.io/cirruslabs/macos-tahoe-base` or the newest base image Cluster Bar offers) | macOS VM `margin-prod`, same image |
| Runs | `margin-service serve` (LaunchAgent), `cloudflared` (LaunchAgent), `manimgx`, `fluidaudiocli` | same |
| Data | `~/Library/Application Support/Margin/{service.toml,data/{db,videos,sources,uploads,models}}` inside the guest | same, plus `…/Margin/backups/` |
| Size | 4 CPU, 8 GB, 80 GB disk | 4 CPU, 12 GB, 160 GB disk (sources up to 2 GB per add, videos, 1.4 GB of models) |
| Later | LiveKit beside the service (Q10 of the live-context plan still prefers a Linux VM for media) | same |

**Why macOS guests.** `crates/embed` is macOS-only (`objc2-core-ml`, `objc2-pdf-kit`, `objc2-vision`, `objc2-av-foundation`), secrets are the login Keychain through `keyring`, scene code renders under `sandbox-exec`, and the service is a LaunchAgent that needs a logged-in GUI session. A Linux guest would need the Linux build the live-context plan defers (Q16). Tart's base images auto-login `admin` with SSH on, which is exactly the session the LaunchAgent needs.

**Two-VM limit.** Apple's Virtualization framework runs at most **two macOS guests per host**. dev and prod take both, so the CI runner lives on the host (§4), not in a third VM. The alternative (prod directly on the Mac mini as today, dev in a VM, the second slot free for throwaway CI VMs) is Q4.

**No Neural Engine in a VM.** Core ML inside a guest has GPU and CPU only; `Embedder` loads with all compute units and will silently fall back. Measure before trusting dev numbers: ingest the eval corpus (`evals/corpus`, ~5 MB) on the host Mac and in each VM, then compare the `embed` trace rows (`GET /api/traces`, `latency_ms` per chunk, or `margin-eval export`). Record host and VM medians in `evals/reports/`. If the VM is more than ~3× slower, prod embedding stays acceptable (sources are ingested once) but the eval runner (§6) should run on the host with the VM only as the trace source.

## 2. Networking

- One Cloudflare Tunnel **per VM**, `cloudflared` running as a LaunchAgent in the guest, forwarding to `127.0.0.1:8787`. No ports open on the Mac mini; the VMs use the default NAT network. TLS terminates at Cloudflare as today.
- Hostnames on the owner's domain `<domain>` (Q1): prod `margin.<domain>` (web, API and apps are one origin), dev `dev.margin.<domain>`; reserved for later `live.margin.<domain>` and `live.dev.margin.<domain>` (LiveKit signalling). `public_url` in each guest's `service.toml` is its hostname.
- Put **Cloudflare Access** (GitHub identity, the owner's login only) in front of dev so the dev service is never reachable by strangers; prod stays as the service's own OAuth allow-list.
- **GitHub OAuth apps**: one per environment ("Margin" and "Margin dev"), because an OAuth app has exactly one callback URL (`https://<host>/auth/github/callback`). Each VM's `service.toml` holds its own `github_client_id`; the secret goes in that VM's Keychain.
- Cloudflare refuses bodies over 100 MB; multipart uploads already handle it (PLAN.md Upload limits).

## 3. Secrets

Each VM has its own login Keychain and the service name stays `com.krishnanelloore.margin` (`secrets.rs`); nothing collides because the Keychains are separate. Items per VM: `github-client-secret`, `openai-api-key` (and the providers brief's Claude key), `media-signing-key` (generated). The owner enters them **in the VM's own window** (`tart run` window or Screen Sharing), never over an SSH session (the login Keychain is locked there) and never in the repo, in GitHub Actions secrets, in Cluster Bar or on a device: `margin-service setup` for the GitHub secret, Settings → AI provider in the app for model keys. Keychain "Always Allow" prompts appear in that window too. To stop the prompt returning on every deploy, CI signs the binary with a **stable identity** (§4); the Keychain ACL matches the signing requirement, so only the first install asks.

## 4. CI/CD: GitHub Actions on a self-hosted runner from Cluster Bar

**Runner.** One runner on the Mac mini host, labels `self-hosted, macOS, arm64, margin`, created and registered by Cluster Bar (registration needs the owner's GitHub session). It has Rust, `dx`, Xcode CLT, and an SSH key for `admin@<vm-ip>` in both VMs (`tart ip margin-dev`). No GitHub Actions secrets are needed: deploys go runner → VM over the host's NAT network.

Workflows (files in §8):

| Workflow | Trigger | Does |
|---|---|---|
| `test.yml` | every push and PR | `cargo test --workspace` (fakes only: in-memory SurrealDB, fake GitHub and Responses servers), `cargo clippy -D warnings`, `cargo check` for `aarch64-apple-ios-sim`. Never reaches a VM, the Keychain or a model API. |
| `build-service.yml` | push to the deploy branch, `workflow_dispatch` | `cargo build --release -p margin-service` with `MARGIN_BUILD_SHA`, `codesign --sign "Margin Service Signing"` (a self-signed identity in the runner's Keychain, made once by the owner), upload the artifact `margin-service-<sha>` |
| `deploy-dev.yml` | after `build-service` on the deploy branch (Q2: `claude/agent-build`) | `deploy.sh dev <artifact>`: backup (§5), copy the binary, `launchctl kickstart -k`, wait for `/health`, check `/api/version` reports `<sha>` and `environment = dev`, smoke-sign-in is left to the owner |
| `promote-prod.yml` | `workflow_dispatch` with the dev run id; GitHub Environment `prod` requires the owner's review | same `deploy.sh prod` with the **same artifact** that ran on dev |
| `apps.yml` | tag or `workflow_dispatch` | desktop: `dx bundle … --package-types macos --release` → signed `.dmg` artifact; Android: `dx build --platform android` → APK artifact; iOS TestFlight later (Apple Developer account, Q7). Builds are per environment through `MARGIN_DEFAULT_SERVICE_URL` (§8). |

The apps pick the environment with the service URL screen they already have (`ui/signin.rs`); the device token is stored per URL (`app/secrets.rs`), so one desktop app can switch between dev and prod without mixing tokens. §8 adds named presets so the owner does not type hostnames.

## 5. Data

- **Backup before every deploy**: `deploy.sh` runs `backup-margin.sh` in the guest first: `launchctl bootout` the service, copy `data_dir` to `backups/<date>-<old sha>/`, start. It refuses while the service runs, keeps the last 10, and is the same script `docs/plan-dogfood.md` §5 asks for (one file, owned here, D calls it).
- **Migrations** run at service start (`db.rs`: `DEFINE … IF NOT EXISTS`, the one-pass `passages-v1` re-embed). A deploy is: backup → new binary → start → `/health` → `/api/version`. Rollback is: stop, copy the backup back, start the previous artifact (kept in the runner's artifact store for 90 days).
- **Prod → dev restore** (`restore-into-dev.sh`, by hand only): copy the newest prod backup into the dev VM's `data_dir`, then delete the `device` rows (prod devices must not authenticate to dev) and the trace rows (dev evals must not read prod usage). Secrets are not copied; dev has its own `media-signing-key`, so old media links die, which is fine. Sources and chats are the owner's own data (single user); still, dev is where agents run, so the restore prints what it copied and the owner confirms.
- **Tests and evals never see prod**: `test.yml` runs on fakes; `margin-eval` takes `--service-url` and refuses `margin.<domain>`; the Orchard/Cluster Bar runner has no SSH key for prod outside `deploy.sh prod`, which only the `prod` environment's approved job can run.

## 6. Traces and evals

The eval runner (`margin-eval run … --service-url https://dev.margin.<domain>`) reads cost, latency and cache numbers from **dev** traces over `GET /api/traces` with the owner's device token from the runner Mac's Keychain, and keeps its own `data_dir/evals/`. Dogfood harvesting (`margin-eval harvest`, dogfood plan §6) also points at dev. The opt-in eval job is `workflow_dispatch` only and runs on the host runner so embedding uses the Neural Engine; §1's measurement says whether the VM numbers are comparable.

## 7. Runbook for a Claude Code agent on the Mac mini

Steps marked **[owner]** need the owner; **[Cluster Bar]** is what it does. Everything else is the agent at a shell.

0. Read Cluster Bar's README and confirm it can: clone and size Tart VMs, run them headless, create a GitHub Actions runner on the host. Note what it cannot do; those steps use the `tart` commands below.
1. **[owner]** Choose `<domain>` (Q1) and add it to Cloudflare if it is not there.
2. **[Cluster Bar]** Create `margin-dev` and `margin-prod` from the macOS base image, sized per §1, headless, default NAT. Fallback: `tart clone <image> margin-dev && tart set margin-dev --cpu 4 --memory 8192 --disk-size 80 && tart run margin-dev --no-graphics`.
3. **[owner]** Tart's licence: confirm it covers this use (free for personal use; check the current terms).
4. In each guest over SSH (`admin@$(tart ip margin-dev)`): `brew install cloudflared`, install `manimgx` and `fluidaudiocli` the way Settings → Tools shows, `mkdir -p ~/.local/bin ~/Library/Logs/Margin`, install `deploy/launchd/*.plist` with the hostname filled in. Change the guest's `admin` password **[owner]**.
5. **[owner, Cloudflare]** In each guest's window: `cloudflared tunnel login` (browser), then the agent runs `cloudflared tunnel create margin-dev`, `route dns margin-dev dev.margin.<domain>`, writes `~/.cloudflared/config.yml` from `deploy/cloudflared/config.yml`, `cloudflared service install`. Same for prod. **[owner]** Cloudflare Access policy on `dev.margin.<domain>`.
6. **[owner, GitHub]** Create the two OAuth apps (callback `https://<host>/auth/github/callback`); keep the client ids and secrets at hand.
7. **[owner]** In each guest's window: `margin-service setup` (public URL, client id, secret → Keychain, login `Nkr1shna`). The binary for this first run comes from step 9's first artifact or a local `cargo build`.
8. **[Cluster Bar]** Create the host runner with labels `margin`; **[owner, GitHub]** approve its registration. The agent puts Rust, `dx`, Xcode CLT on the runner, makes the SSH key, adds it to both guests' `authorized_keys`. **[owner]** Create the self-signed identity "Margin Service Signing" in the runner's login Keychain (Keychain Access → Certificate Assistant) and allow `codesign` to use it.
9. Push the §8 branch; `test.yml` and `build-service.yml` run; `deploy-dev.yml` installs the binary in dev. **[owner]** In the dev window, click "Always Allow" on the first Keychain prompt. Check `curl https://dev.margin.<domain>/api/version`.
10. **[owner]** Open the desktop app, pick **Dev**, sign in with GitHub, enter the model keys in Settings, download the embedding model. Run the §1 embedding measurement.
11. **[owner, GitHub]** Create Environment `prod` with "Required reviewers: owner". Run `promote-prod` for the dev run; approve it; repeat steps 9–10's checks on prod.
12. Decide Q9 (prod data): if prod takes the owner's library, stop the service on his Mac, `backup-margin.sh`, copy the backup into the prod guest, start, and point the app at **Prod**.
13. Schedule `backup-margin.sh` nightly in each guest (`launchd` StartCalendarInterval) and `tart export` of the prod VM weekly to an external disk **[owner: which disk]**.

## 8. What changes in the repo (area E-deploy, one worktree, exclusive files)

- `deploy/scripts/{provision-guest.sh,deploy.sh,backup-margin.sh,restore-into-dev.sh}`, `deploy/launchd/{com.krishnanelloore.margin.service,com.krishnanelloore.margin.backup}.plist`, `deploy/cloudflared/config.yml` (templates with `<HOST>`, `<UUID>`), `.github/workflows/{test,build-service,deploy-dev,promote-prod,apps}.yml`.
- `crates/service/src/config.rs`: `environment: String` (`"dev" | "prod"`, default `"prod"`), asked by `setup`. `crates/service/src/version.rs` (new): `GET /api/version` → `{ version, sha (from MARGIN_BUILD_SHA), environment, started_at }`; one route line in `server.rs` and `routes.rs` (coordinate: both shared files, two lines each, added in the first commit). `/health` stays `ok` for the tunnel.
- `crates/app/src/config.rs`: `DEFAULT_SERVICE_URL` from `option_env!("MARGIN_DEFAULT_SERVICE_URL")`, plus `pub const PRESETS: [(name, url)]` for the sign-in screen's Prod / Dev / Custom choice (`ui/signin.rs`, one new row). Nothing else in the app changes; tokens are already per URL.
- `crates/evals/src/bin/margin-eval.rs`: `--service-url` required for `run`/`harvest`/`export`, refusing the prod hostname. `docs/service-setup.md`: a note that a VM follows the same steps, pointing here. `docs/HANDOFF.md`: an E row.
- No change to data layout, Keychain names, auth, or the wire contract beyond the one route.

## 9. Owner questions, with recommendations

1. **Domain.** Which domain, and is it on Cloudflare? Recommend `margin.<domain>` for prod, `dev.margin.<domain>` for dev, LiveKit under `live.`.
2. **Which branch deploys to dev?** Recommend `claude/agent-build` now (it is where every phase merges); switch to `main` once `main` is caught up (it is still at `dbd08ff`).
3. **What promotes to prod?** Recommend the exact artifact that ran on dev, by run id, with the owner's approval in the `prod` GitHub Environment; no prod builds from source.
4. **Two VMs, or prod on the host?** Recommend two VMs as asked (clean, restorable with `tart export`); the host-prod layout only if CI needs throwaway macOS VMs later.
5. **VM sizes.** What are the Mac mini's cores, memory and disk? Recommend dev 4/8 GB/80 GB, prod 4/12 GB/160 GB, leaving at least 4 GB and 2 cores for the host and runner.
6. **Cloudflare Access on dev** (GitHub identity, owner only)? Recommend yes; prod stays on the service's own allow-list.
7. **App builds.** Desktop `.dmg` and Android APK from CI now; iOS TestFlight needs the Apple Developer account and a distribution certificate on the runner: now or after phase 5? Recommend after phase 5.
8. **Signing identity.** Self-signed "Margin Service Signing" (free, one Keychain prompt per VM) or a Developer ID (needed anyway for TestFlight)? Recommend self-signed now.
9. **Does prod hold the owner's real library now?** Recommend not yet: keep the live instance on his Mac until dev has run for two weeks and the §1 embedding measurement and one restore drill have passed; then move it (runbook step 12) and retire the Mac's service.
10. **Prod → dev restores**: on demand only (recommended), or nightly?
11. **Who else may build?** The runner trusts pushes to the repo; recommend branch protection so only the owner's merges reach the deploy branch, and `pull_request` from forks never runs on the self-hosted runner.
