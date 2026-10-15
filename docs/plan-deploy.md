# Deploy plan: dev and prod on the Mac mini

Written 2026-10-14 on `claude/agent-build` (`af8cbe6`), updated the same day with the facts of the Mac mini setup and built as area E-deploy (`agent/e-deploy`). Read: `docs/service-setup.md`, `PLAN.md`, `docs/plan-dogfood.md` §5, `docs/plan-live-context.md` §5.2 and Q10/Q16, `crates/service/src/{config,version,restore,main}.rs`, `deploy/`, `.github/workflows/`, `evals/README.md`. First written for macOS guests (launchd, the login Keychain); the guests are now Ubuntu 24.04 with systemd (`docs/plan-linux-service.md` §5), the macOS service paths are removed from the repo, and the scripts stop on any guest that isn't Linux.

**Cluster Bar / Orchard.** The VMs are Linux Tart guests (the `margin-linux` image, `deploy/image`) that Orchard schedules on the Mac mini, driven from Cluster Bar. Orchard's worker names each guest's disk `orchard-<name>-<uid>-<restart count>` and runs `tart clone <image>` for every new name (`internal/worker/vmmanager/tart/tart.go`, `cloneAndConfigure`; `internal/worker/ondiskname`). So **every restart under "restart on failure", every reschedule and every recreate starts from a fresh clone of the base image: the guest's own disk is thrown away.** §5 is built around that.

## 1. Environments

| | dev | prod |
|---|---|---|
| Host | Mac mini, M1, 16 GB | same |
| Guest | Ubuntu 24.04 Tart VM `margin-dev` (Orchard, Cluster Bar, image `margin-linux`) | Ubuntu 24.04 Tart VM `margin-prod` |
| Size | 4 CPU, **4 GB**, 60 GB disk | 4 CPU, 8 GB, 100 GB disk |
| Reached as | `admin@margin-dev.local` (DHCP address changes; the mDNS name doesn't), key `~/.ssh/margin_deploy` on the host | `admin@margin-prod.local`, same key |
| Public URL | `https://devmargin.krishnanelloore.com`, tunnel `margin-dev` | `https://margin.krishnanelloore.com`, tunnel `margin-prod` |
| Runs | `margin-service serve` (systemd `margin-service`), `cloudflared` (systemd `margin-cloudflared`), the nightly backup (`margin-backup` timer), `manimgx`, `margin-tts`, `bwrap` | same |
| Persistent | host `~/MarginVMs/margin-dev` → guest `/mnt/shared/margin` (virtiofs, mounted by the startup script) | host `~/MarginVMs/margin-prod` → same guest path |
| Later | LiveKit at `livedevmargin.krishnanelloore.com` | `livemargin.krishnanelloore.com` |

**Why Linux guests.** The service is a Linux build (`docs/plan-linux-service.md`): ONNX Runtime on the CPU, pdfium, tesseract, ffmpeg, Parakeet, bubblewrap, a secrets file and a systemd unit; nothing needs a GUI session. Apple's Virtualization framework allows only two macOS guests per host; Linux guests have no such limit, so dev, prod and the Linux CI runner (`margin-ci`) are each a VM.

**Memory.** The M1 has 16 GB: 4 for dev, 8 for prod, the rest for the host, `margin-ci` and builds. Dev's 4 GB must hold the embedding model (EmbeddingGemma ONNX 8-bit), the speech model while it transcribes, SurrealDB's caches and the service (the unit's `MemoryMax`). To make that visible: `GET /api/version` reports `rss_bytes` (the service's resident memory) and `physical_memory_bytes` (the VM's), `deploy.sh` prints the RSS after each deploy, and the service logs a warning at start when the machine has less than 6 GB (`version::LOW_MEMORY_BYTES`), which dev always does. If dev swaps under an ingest (watch `rss_bytes`), the fixes are: dev to 6 GB (prod to 6 GB too), or not loading the embedder in dev until first use.

## 2. Networking

- One Cloudflare Tunnel per VM, `cloudflared` as a systemd unit in the guest, forwarding to `127.0.0.1:8787`. No ports open on the Mac mini; the VMs use Orchard's default NAT network. TLS terminates at Cloudflare.
- **Single-level subdomains** (`devmargin.`, `margin.`, later `livemargin.`, `livedevmargin.`) so Cloudflare's free universal certificate covers them; `dev.margin.` would need an Advanced Certificate. `public_url` in each guest's `service.toml` is its hostname. The prod hostname used to route to the tunnel on the owner's Mac (`docs/service-setup.md`); `cloudflared tunnel route dns -f margin-prod margin.krishnanelloore.com` moves it, and the Mac's own tunnel then serves nothing until Q9.
- Put **Cloudflare Access** (GitHub identity, the owner's login only) in front of dev so the dev service is never reachable by strangers; prod stays as the service's own OAuth allow-list. With Access on, `/api/version` on the dev hostname needs the Access cookie; `deploy.sh` checks it inside the guest instead.
- **GitHub OAuth apps**: one per environment ("Margin" and "Margin dev"), because an OAuth app has one callback URL (`https://<host>/auth/github/callback`).
- Cloudflare refuses bodies over 100 MB; multipart uploads already handle it (PLAN.md Upload limits).

## 3. Secrets

Each VM keeps its secrets in `secrets.toml` on its share (0600; `service.toml`'s `secrets_file` points at it; `secrets::FileStore`), so a recreated VM keeps them. Names per VM: `github-client-secret`, `openai-api-key` (and the providers brief's Claude key and the ChatGPT sign-in), `media-signing-key` (generated by the service on first start). `deploy.sh` writes the GitHub client secret (and the OpenAI key when set) from the GitHub Environment's secrets (`MARGIN_SECRET_*`, `common.sh write_secrets`); model keys can also be entered in Settings → AI provider in the app. Never in the repo, Cluster Bar or a device.

GitHub Actions holds the per-Environment secrets above and nothing else: deploys go runner → VM over the host's NAT network with the host's key, the values reach the guest over SSH's stdin (never argv or the log), and no workflow prints a secret.

## 4. CI/CD: GitHub Actions on the host's runner

**Runner.** `~/actions-runners/margin` on the Mac mini, name `mac-mini-host`, labels `self-hosted, macOS, ARM64, margin`; Rust 1.99, `dx` 0.7.10, Xcode 27, `gh`. A second runner on the host serves another repository with the label `clusterbar`, so **every job on the host targets `[self-hosted, macOS, ARM64, margin]`**. The runner user holds `~/.ssh/margin_deploy`, the one deploy key for both VMs. **Linux runner:** `margin-ci`, an Orchard VM (4 CPU / 6 GB / 80 GB plus 4 GB swap, `--os linux`) from the image `margin-linux-v1` built by `deploy/image/margin-linux.pkr.hcl` (Packer + Tart, Ubuntu 24.04 with Rust, clang/libc++, libheif, tesseract, ffmpeg, bubblewrap, pdfium, uv + manimgx, cloudflared, gh and the runner). Its startup script (`deploy/ci/margin-ci-startup.sh`) mounts the host share `~/MarginVMs/margin-ci`, where the runner's files and registration live (so a re-clone keeps them), and starts it under systemd; work folder and Cargo cache stay on the VM disk. Registered once with `deploy/ci/register-margin-ci.sh`.

| Workflow | Trigger | Does |
|---|---|---|
| `ci.yml` (**CI**) | pull requests from this repository (CI never runs on push), `workflow_dispatch` | `linux` on **margin-ci** (`[self-hosted, Linux, ARM64, margin]`): `cargo fmt --check`, clippy and `cargo test` for the workspace minus `margin-app`; `macos` on the Mac host: `margin-app` clippy, tests and the iOS simulator check; each job's first step reads the PR's files and, if its platform isn't touched, skips the rest and passes in seconds (service crates → Linux, `crates/app` → macOS, `crates/shared`/`Cargo.*`/`rust-toolchain.toml`/`ci.yml` → both, docs and deploy scripts → neither); both are required checks (**CI / Linux**, **CI / macOS**) in the repository ruleset |
| `cancel-closed-pr-ci.yml` (**CI cleanup**) | a PR merged or closed | cancels that PR's unfinished CI runs (on a GitHub-hosted runner, so it never waits behind them); a new push to an open PR already cancels the previous run through `ci.yml`'s concurrency |
| `image.yml` | a merge into `dev` that changes `deploy/image/**` or `rust-toolchain.toml`, `workflow_dispatch` | on the Mac host: `packer build deploy/image/margin-linux.pkr.hcl` (Ubuntu 24.04 + the pinned Rust) and `tart push` to **`ghcr.io/nkr1shna/margin-linux`** (`<date>-<sha>` and `latest`) with the job's `GITHUB_TOKEN`; the image holds no secrets |
| `build-service.yml` | a merge into `dev` or `release` that changes the service (paths: `crates/{service,agent,ai,embed,video,shared}`, `Cargo.toml`/`Cargo.lock`, `deploy/`, its workflows), `workflow_dispatch` | no test job (the PR's CI checks already passed on a branch kept up to date with the base); on **margin-ci**: `cargo build --release --locked -p margin-service` (lld, Cargo cache named after the environment) with `MARGIN_BUILD_SHA`; checks it is a Linux aarch64 binary stamped with that commit; uploads `margin-service-<sha>`. Linux binaries aren't code-signed. |
| `deploy-dev.yml` | after a successful `build-service` on `dev` (push or dispatch, this repository), or `workflow_dispatch` with a build run id | `MARGIN_DEPLOY_ONLY=dev`; `deploy.sh dev`: preflight, copy the binary, stop, backup (§5), swap, start, wait for `/health`, check `/api/version` reports `<sha>` and `dev`, roll back to the previous binary if not. Uploads the installed bytes again as `deployed-dev-<sha>`. GitHub Environment `dev` (no reviewer; a deployment record) |
| `deploy-prod.yml` | after a successful `build-service` on `release` (push or dispatch, this repository), or `workflow_dispatch` with a build run id of `release` | Environment **`prod`** (admits only the `release` branch); `MARGIN_DEPLOY_ONLY=prod`; `deploy.sh prod` with that build's artifact; uploads `deployed-prod-<sha>`. Shipping = merging a promote-release PR into `release` (§4a). |
| `smoke-dev.yml` | after a successful `deploy-dev`, or `workflow_dispatch` with a deploy-dev run id | on the Mac host, no secrets: `/health`; `/api/version` reports the deployed sha (from that run's `deployed-dev-<sha>` artifact) and `dev`; protected routes answer 401 signed out and to a made-up token. Records the result as the commit status **`smoke-dev`** on the deployed sha (what promote-release reads), comments it on every PR merged into dev since the previous dev deploy; fails if a check fails (§4a) |
| `promote-release.yml` | `workflow_dispatch` (`sha`, empty = newest green dev commit); a push to `release` | on **margin-ci**: picks a dev snapshot whose `smoke-dev` passed, creates `promote/<short-sha>` at that exact commit and opens a PR into `release` listing the dev PRs it brings; after it is merged (merge commit), checks release equals that commit, tags it `release-<date>-<short-sha>`, labels the PRs `shipped` and opens a PR into dev that regenerates `docs/RELEASES.md` (§4a) |
| `revert-pr.yml` | `workflow_dispatch` (`pr`) | on **margin-ci**: opens a PR into dev, labelled `revert`, that reverts a merged dev PR's commit (revert-on-red, §4a) |
| `apps.yml` | a merge into `dev` or `release` that changes the app (paths: `crates/{app,shared}`, `Cargo.toml`/`Cargo.lock`, `apps.yml`), tag `v*`, or `workflow_dispatch` (prod, dev or both; artifacts only) | per environment, `MARGIN_DEFAULT_SERVICE_URL` set (the build is pinned to it, §10): `dx bundle -p margin-app --platform desktop --package-types macos --release` → ad hoc signed `.dmg`; `dx build -p margin-app --platform android --release` → APK (skipped while the runner has no Android SDK). Published as GitHub releases: rolling pre-release `dev-build` after each dev deploy, `prod-<sha>` (latest) after each promotion, the tag's name for `v*`. iOS TestFlight waits on the Apple Developer account (Q7). |

Cargo builds keep their output in `~/.cache/margin-ci/target-{test,release}` on the runner (outside the checkout, which `actions/checkout` wipes), so CI and CD rebuild only what changed. Prune it if the disk fills (`rm -rf ~/.cache/margin-ci`; the next run rebuilds once).

**Check Linux CI locally before pushing.** `deploy/local/linux-check.sh` (any Apple-silicon Mac with Tart) clones `ghcr.io/nkr1shna/margin-linux:latest` into a throwaway VM, shares the checkout in read-only and runs `deploy/ci/linux-ci.sh` — the same script CI / Linux runs — then deletes the VM; `--release` also builds `margin-service`. Build output and crates are cached in `~/.cache/margin-linux-check`. The package must be public for an anonymous `tart pull` (GitHub → the package → Package settings → Change visibility), else `tart login ghcr.io` with a token that can read packages.

**Keeping the deploy key and prod safe.** `deploy.sh` and `provision-guest.sh` refuse any environment but `MARGIN_DEPLOY_ONLY` when it is set, so a deploy-dev job never opens a connection to margin-prod. Prod is reachable only from the `prod` Environment's job, which waits for the owner's approval. What protects the scripts themselves is that only the owner's commits reach the deploy branch; the owner applies these settings **[owner, GitHub]**:

1. Settings → Actions → General → "Fork pull request workflows from outside collaborators": **Require approval for all outside collaborators**; and keep the repository private. `ci.yml` also skips pull requests whose head is a fork, so fork code never runs on the self-hosted runner.
2. Branches: **`dev`** (the default branch; every push builds and deploys dev) and **`release`** (every push builds and deploys prod; ship by merging a promote-release PR with a merge commit, §4a). Branch protection and required reviewers need a paid plan on a private repository, so the guard is the Environments below plus the owner being the only one who can push.
3. Settings → Environments → `prod`: deployment branches = **Selected branches: `release`**, so only a build of `release` can reach prod (deploy-prod.yml also checks the build run's branch).
4. Settings → Environments → `dev`: deployment branches = `dev`.
5. Settings → Actions → General → Workflow permissions: **Read repository contents** (the default token stays read-only).
6. Merge methods: **dev PRs squash, release PRs merge-commit** (§4a). A squash-only repository can't release: a squash-merged release PR is a new commit that isn't on dev, so release would stop being an exact dev snapshot. Settings → General → Pull Requests: turn on **Allow merge commits** and **Allow squash merging** (squash default message: pull request title; rebase merging off). Then pin each branch to its method in Settings → Rules → Rulesets (the ruleset that already requires CI, or one per branch): for `dev`, "Require a pull request before merging" → Allowed merge methods: **Squash** only; for `release`, Allowed merge methods: **Merge** only, and don't turn on "Require branches to be up to date" for `release` (the release PR's branch is a dev commit, which never contains release's merge commits). Without the rulesets, pick the right button by hand; promote-release still refuses to tag a squashed release (§4a).
7. Settings → Actions → Runners: the `mac-mini-host` runner in the default group, used only by this repository.

Each app build is pinned to its environment's service (§10): "Margin" to prod, "Margin Dev" to dev, with no picker; only a developer build asks for an address. The device token is stored per URL (`app/secrets.rs`) and each app keeps its own settings and Keychain entries, so both install side by side without mixing tokens.

## 4a. Releasing: prod gets a verified dev snapshot

**The model: trunk-based, ship snapshots.** `dev` is the trunk and is kept shippable at
every commit. A release is not a hand-picked set of PRs; it is **dev exactly as it was at
one commit whose smoke test passed**, together with every PR merged before it. So a PR that
broke something and the later PR that fixed it ship together automatically: pick a green
commit after the fix and both are in it, with nothing to track or cherry-pick. `release`
only ever moves to a dev commit, so prod runs code dev already ran.

**Keeping dev shippable (the rules).**

1. **Required CI before merge.** Every PR passes **CI / Linux** and **CI / macOS** (the
   ruleset) on a branch kept up to date with `dev`.
2. **Squash merge into dev.** One PR = one commit on dev's first-parent line; that line is
   what releases are cut from and what the release PR lists.
3. **smoke-dev after each deploy.** `build-service` → `deploy-dev` → **`smoke-dev`** checks
   devmargin (`/health`, `/api/version` sha and environment, signed-out requests refused),
   comments the result on the PRs the deploy brought, and records it as the commit status
   **`smoke-dev`** (success/failure) on the deployed dev commit. It does not test features.
4. **Revert on red.** When smoke-dev fails, or a merged PR misbehaves on devmargin, revert it
   first and fix forward: Actions → **revert-pr** → Run workflow with the PR number opens a
   PR into dev, labelled `revert`, that reverts the PR's commit; squash-merge it when CI
   passes, then bring the change back as a new PR with the fix. Don't leave dev red to "fix
   later": while the tip is red, releases can only come from an older green commit.
5. **Unfinished features ship dark: label them `needs-flag`.** Such a PR adds a setting that
   keeps the feature off unless enabled (a field in the settings API, or a `service.toml`
   key for service-wide behaviour), defaulting to off; dev turns it on to test. Because a
   release takes all of dev, this is how half-built work merges early without reaching
   users: prod gets the code with the feature off. The release PR marks such PRs "ships
   dark", and the owner turns the setting on in prod when ready. A later PR removes the
   flag once the feature is on everywhere.
6. **`verified-on-dev` (optional).** After trying a PR on devmargin the owner may label it
   `verified-on-dev`; the release PR shows each PR's labels, so an untried PR is visible
   before merging. It gates nothing; a PR that turns out broken is reverted (rule 4).

**Shipping.**

1. Actions → **promote-release** → Run workflow. Leave `sha` empty for the newest dev
   commit whose `smoke-dev` passed (newer commits that are red or were never deployed are
   skipped and named in the log), or give a dev commit. It **refuses, naming the commit**,
   when that commit's smoke-dev failed or is missing, when it isn't a commit on dev's
   first-parent line, or when it is already on release. A commit that changes only docs or
   only the app doesn't deploy the service (none of build-service's paths), so it has no
   smoke-dev status of its own: it takes the result of the nearest earlier commit that
   changed the service, and if no commit since the last release did, the service is what
   prod already runs and it is accepted (the log says which commit's result it relies on).
   A commit that changes the service must have its own passing status: if the tip is
   undeployed, run **build-service** on `dev` (Run workflow), which deploys and smoke-tests
   it.
2. The workflow (`deploy/scripts/release-snapshot.sh` holds the logic) finds the last dev
   commit release has, `git merge-base origin/release origin/dev`; creates branch
   **`promote/<short-sha>` at the chosen commit** (no new commit); and opens a PR into
   `release` listing every dev PR merged between the two (GitHub's commit → PR lookup),
   with titles, labels and which ship dark.
3. **Merge that PR with "Create a merge commit"**, never "Squash and merge". The merge
   commit's files are exactly the snapshot's, and the snapshot becomes release's newest dev
   commit, so the next merge-base (and the next PR list) starts there. A fast-forward
   (`git push origin <sha>:release`) is equally correct. A squash would put a new commit on
   release that dev doesn't have: the merge-base would stay behind and the next release PR
   would list already-shipped PRs again.
4. The push to `release` deploys prod (build-service → deploy-prod), and promote-release's
   `released` job checks that release's files equal the snapshot (a squash merge or a
   direct push fails here and isn't tagged), tags it **`release-<date>-<short-sha>`** (the
   annotated tag records the dev sha, the previous one and the PRs), labels those PRs
   **`shipped`** with a comment, and opens a PR into dev that regenerates
   `docs/RELEASES.md`; merge that one when its CI passes.

**Hotfixes** go through dev like everything else: fix (or revert) on dev, wait for its
smoke-dev, release that commit. Nothing is committed to `release` directly.

**The ledger is generated.** `docs/RELEASES.md` is written by
`deploy/scripts/releases-ledger.sh` from the `release-*` tags (each: tag, dev sha, PRs
included). dev takes only PRs, so a ledger change is a PR either way; generating it means a
missed run is fixed by the next one, and it can be rebuilt any time:
`git fetch --tags && sh deploy/scripts/releases-ledger.sh > docs/RELEASES.md`.

**Token.** Without the optional repository secret `PROMOTE_TOKEN` the workflows use their
`GITHUB_TOKEN`, which can open PRs only with **[owner, GitHub]** Settings → Actions →
General → "Allow GitHub Actions to create and approve pull requests". PRs opened by
`GITHUB_TOKEN` trigger no workflows, so promote-release and revert-pr dispatch `ci.yml` on
their branch (the same `Linux`/`macOS` checks the ruleset requires). If GitHub refuses
`GITHUB_TOKEN` creating the `promote/` branch or the tag at a commit that changed
`.github/workflows/`, or pushing a revert of such a PR, add `PROMOTE_TOKEN`: a fine-grained
token for this repository with Contents, Pull requests and Workflows read/write. With it,
the PRs also trigger CI as usual.

**The first release** is a dev snapshot like any other; there is no special block step.
`release` is at `85b861f`, which dev contains, so the merge-base is `85b861f` and the first
release PR lists everything since (PRs #1–#16 and on; the early ones were merged with merge
commits, but each is still one commit on dev's first-parent line). It needs a dev commit
with a `smoke-dev` status, which smoke-dev records from this change on: after it merges,
run build-service on `dev` once if no deploy has happened since. Merge the release PR with a
merge commit (or fast-forward), never a squash.

Check the selection, PR-list, release-check and ledger logic locally (a throwaway
repository and a fake `gh`, no network): `sh deploy/scripts/release-snapshot-test.sh`.

## 5. Data: what persists, what is wiped

**Deploys never wipe the database.** A deploy replaces one file, `bin/margin-service`; `data/`, `backups/` and `service.toml` are never written by it except for the backup copy it takes first.

**Orchard recreation would wipe it**, if it were on the guest's disk (see the top). So everything that must survive lives in one host folder per VM, shared into the guest with Orchard `hostDirs` (Tart `--dir`):

```
orchard create vm margin-dev  --os linux --image margin-linux-v1 --cpu 4 --memory 4096 --disk-size 60 \
  --restart-policy OnFailure --host-dirs margin:/Users/<owner>/MarginVMs/margin-dev \
  --startup-script @<(deploy/scripts/provision-guest.sh --startup-script dev)
orchard create vm margin-prod --os linux --image margin-linux-v1 --cpu 4 --memory 8192 --disk-size 100 \
  --restart-policy OnFailure --host-dirs margin:/Users/<owner>/MarginVMs/margin-prod \
  --startup-script @<(deploy/scripts/provision-guest.sh --startup-script prod)
```

(or the same fields in Cluster Bar). The startup script mounts the virtiofs share at `/mnt/shared` (`/mnt/shared/margin`).

| Where | What | Survives a deploy | Survives Orchard recreating the VM |
|---|---|---|---|
| Share (`~/MarginVMs/margin-<env>` on the Mac mini) | `.margin-persistent` (marker), `service.toml`, `secrets.toml`, `data/` (`db/`, `sources/`, `uploads/`, `videos/`, `models/`), `backups/`, `bin/` (the installed binary, the previous one, the scripts), `cloudflared/` (`cert.pem`, tunnel credentials, `config.yml`; `~/.cloudflared` links here), `templates/` | yes (only `bin/margin-service` changes) | yes |
| Guest disk | Ubuntu and the image's tools (`cloudflared`, `manimgx`, tesseract, ffmpeg, bubblewrap, pdfium), the systemd units, `margin-tts` in `~/.local/bin`, the journal, the guest's SSH host key | yes | **no**: the startup script reinstalls the units at every boot; `provision-guest.sh <env> --rekey` takes the new host key; `margin-tts` is installed again from Settings → Tools |
| Orchard startup script | hostname `margin-<env>` (so `.local` resolves), the deploy public key in `authorized_keys`, the share mount, the units | — | re-applied at every start |

**Guards.** `deploy.sh` refuses to deploy unless, inside the guest: the guest is Linux with systemd, the share is mounted at `/mnt/shared/margin`, it is another file system than the guest's root disk, its marker names this VM, `service.toml` on it says this environment, and `data_dir` is on the share. The service's unit checks the marker before it starts the binary (which itself lives on the share), so a guest that boots before the share mounts never creates a fresh database on its own disk.

**Backups** live on the share, which is the Mac mini's disk, so they survive the VM too:
- `deploy.sh` runs `backup-margin.sh --check --label before-<new sha>-from-<old sha>` after stopping the service and before swapping the binary. It refuses while the service runs, writes `backups/<date-time>-<label>/data`, and keeps the newest 10. The check runs with the **new** binary (`MARGIN_SERVICE_BIN=bin/margin-service.new`, since the old build may predate `check-data`); a copy that fails it is deleted and the deploy stops before the swap, with the old build started again.
- The `margin-backup` systemd timer runs `backup-margin.sh --stop-service --label nightly --check` at 03:30 (the unit fails, visibly in `systemctl --failed` and the journal, when the copy fails its check).
- `deploy/scripts/backup-margin.sh` is the one backup script: the same file `docs/plan-dogfood.md` §5 asks for (it moved from `scripts/`). `margin-service backup [--label NAME]` is the same backup in Rust (copy, rename, check, keep 10), which the service also takes by itself before a schema migration (below).
- **Checked backups** (phase 5): `margin-service check-data <dir>` copies `<dir>/db` to a temporary folder (a backup is never opened in place), opens the copy without defining or migrating anything, lists `SettingsRow.migrations` and the schema version, counts the rows of every table, and checks that every Ready source and file-backed document has its `file` under `<dir>/sources` or `<dir>/documents` and that every passage's document exists; it prints `count <table> <n>` lines, the problems, a last one-line verdict (`OK …` / `FAILED …`), and exits 1 on any problem or missing table. `--check` runs it on the fresh copy and writes the report to `backups/<name>/CHECKED`; a copy that fails is deleted. **No backup folder without `CHECKED` is trusted**: the restore drill and the off-site copy take only checked ones.
- Off the Mac mini: `deploy/scripts/offsite-backup.sh DEST [--env prod|dev|all]` copies every checked backup of `~/MarginVMs/margin-<env>/backups` (default prod) to `DEST/margin-<env>/` with rsync, never deleting there, `CHECKED` last so a folder on `DEST` with `CHECKED` is whole, verifies each copy (a second rsync pass changes nothing) and writes `DEST/margin-<env>/LAST_COPY`; it refuses a `DEST` that doesn't exist (a disk not mounted). `offsite-backup.sh --install DEST` installs it as a weekly LaunchAgent on the host (Sundays 04:30, template `deploy/launchd/com.krishnanelloore.margin.offsite-backup.plist`, log `~/Library/Logs/Margin/offsite-backup.log`) — the destination is **[owner: which disk]** (runbook 13).

**Migrations** run at service start (`db.rs`: `DEFINE … IF NOT EXISTS`, the one-pass `passages-v1` re-embed). A deploy is: backup → new binary → start → `/health` → `/api/version`. A build that doesn't come up is rolled back to `bin/margin-service.prev` automatically; data is restored by hand from the printed backup when the failed build had migrated it.

**Schema guard** (phase 5). The settings row records `schema_version`, the `db::SCHEMA_VERSION` of the build that last opened the data (bumped when a `DEFINE` or a one-pass migration changes). Every start (`serve`, `scrub-restored`) first reads it without defining or migrating anything (`restore::open_guarded`): data a **newer** build wrote is refused ("This data was last used by a newer Margin (schema N); install that build or restore the backup from before it.") and the unit keeps failing until a newer build is deployed or a backup restored; data an **older** build wrote (or a row from before the field) is backed up first (`margin-service backup --label before-schema-<n>`: checked, kept by the same rule), then opened and migrated as before; a failed backup refuses to start, so a migration never runs without a backup beside it. A by-hand start is as safe as a deploy; a deploy that migrates makes two backups (deploy.sh's and the service's), which the keep rule prunes.

**SurrealKV on a shared folder.** The database now sits on virtio-fs, not the guest's own disk. The restore drill (runbook 12) and a hard stop (`restore-drill.sh dev --hard-stop`: a long document is ingested, the service is killed under it, then `/health` and a search) must pass before prod holds the owner's library; if SurrealKV misbehaves on virtio-fs, the alternative is the database on the guest disk with `backup-margin.sh` every hour to the share, accepting up to an hour's loss on recreation. The hard stop kills the process (`systemctl kill --signal=SIGKILL`); a VM-level stop is `MARGIN_HARD_STOP_CMD="<the Orchard restart of margin-dev>" restore-drill.sh dev --hard-stop` (run on the host; the drill waits for SSH to come back), and after an Orchard re-clone the guest needs `provision-guest.sh dev --rekey` before the service comes back (runbook 11).

**Restore drill of dev** (`deploy/scripts/restore-drill.sh dev [BACKUP] [--hard-stop]`, on the Mac mini by hand or from the `restore-drill` workflow, `workflow_dispatch` on the host runner with `MARGIN_DEPLOY_ONLY=dev`; it refuses any other environment): takes the newest **checked dev** backup (or `BACKUP`), runs `check-data` on it in the guest for the expected counts, restores it into dev as `restore-into-dev.sh` does (pre-restore backup `pre-drill` with `--check`, swap with dev's data kept as `data.before-drill-<time>`, `scrub-restored`, start), then on dev's public URL checks `/health`, `/api/version` (`environment: dev`), signs in afresh (it prints the GitHub URL to open and waits up to 10 minutes: the scrub removed every device), compares `GET /api/library` (topics, lessons, notes, videos, sources outside folders) with the backup's counts, runs one `POST /api/context/search` (which must find passages when the backup has some), and removes its device again. The result is `backups/drills/<date-time>.txt` in dev's share (and the job summary); the script exits 1 if any check failed. Dev then runs on the restored backup: what it had since is in the `pre-drill` backup and `data.before-drill-<time>`. Monthly, by hand (runbook 12).

**Prod → dev restore** (`restore-into-dev.sh`, by hand only, refuses CI and needs a terminal): copies the newest prod backup on the host from `~/MarginVMs/margin-prod/backups` (no SSH to prod at all), asks the owner to type `restore`, stops dev, backs dev up (`pre-restore`), puts the copy in dev's `data/` (dev's old `data/` kept beside it as `data.before-restore-<time>`), runs `margin-service scrub-restored` (deletes every `device` row so prod devices can't sign in to dev, and every trace row and kept rollup so dev evals don't read prod usage; refuses a config whose environment isn't dev), and starts dev. Secrets aren't copied.

**Tests and evals never see prod**: CI (`ci.yml`) runs on fakes; `margin-eval` requires `--service-url` for `run`, `export` and `report --service`, refuses the host `margin.krishnanelloore.com` when parsing, and refuses any service whose `/api/version` says anything but `dev` (which also catches the owner's own Mac, whose config predates `environment` and so is `prod`); the runner reaches prod only from the approved `prod` job.

## 6. Traces and evals

The eval runner (`margin-eval run … --service-url https://devmargin.krishnanelloore.com`) reads cost, latency and cache numbers from **dev** traces over `GET /api/traces` with the owner's device token, and keeps its own `data_dir/evals/`. Dogfood harvesting (`margin-eval harvest`, dogfood plan §6) also points at dev and must take `--service-url` the same way (`Command::needs_service_url`, `cli::check_service`). With Cloudflare Access on dev, the runner uses the service in the VM directly (`http://margin-dev.local:8787` is not reachable: the service binds 127.0.0.1) or an Access service token; owner question 6 decides. The opt-in eval job is `workflow_dispatch` only; the runner embeds with the same ONNX backend the service runs.

## 7. Runbook for a Claude Code agent on the Mac mini

Steps marked **[owner]** need the owner. Everything else is the agent at the host's shell, in a checkout of the deploy branch.

0. Done by the owner already: Cloudflare zone `krishnanelloore.com`; tunnels `margin-dev` and `margin-prod`; VMs `margin-dev` (4/4 GB/60 GB) and `margin-prod` (4/8 GB/100 GB) from the `margin-linux` image (`deploy/image`) under Orchard with restart on failure; the runner `mac-mini-host`; `~/.ssh/margin_deploy`.
1. `deploy/scripts/provision-guest.sh dev` once on the host: it creates `~/MarginVMs/margin-dev` with its marker and stops, saying the share isn't mounted. **[owner, Cluster Bar]** Add the shared folder `margin:/Users/<owner>/MarginVMs/margin-dev` (Orchard `hostDirs`) and the startup script from `provision-guest.sh --startup-script dev` to `margin-dev`; Orchard restarts it (a fresh clone). Same for prod.
2. If a VM already holds something on its own disk (a tunnel login, a `service.toml`, a database), copy it out **before** the owner adds the share in step 1, because that restart is a fresh clone: stop the service in the guest, then from the host `scp -r -i ~/.ssh/margin_deploy 'admin@margin-dev.local:.cloudflared/*' ~/MarginVMs/margin-dev/cloudflared/` and `scp -r … 'admin@margin-dev.local:/var/lib/margin/data' 'admin@margin-dev.local:/etc/margin/service.toml' ~/MarginVMs/margin-dev/`, then set `data_dir` in the copied `service.toml` to `/mnt/shared/margin/data` and add `environment = "dev"`.
3. `provision-guest.sh dev --rekey` (the restarted VM has a new host key): links `~/.cloudflared` to the share, writes a starter `service.toml` (environment, public URL, `data_dir` and `secrets_file` on the share), installs the systemd units and starts the backup timer. It prints what is left.
4. In the guest over SSH: install `margin-tts` the way Settings → Tools shows (the image has `manimgx` and the readers; repeat after a recreation). **[owner]** Change the guest's `admin` password from the image default and update it in Orchard's VM spec.
5. **[owner]** `cloudflared tunnel login` if the share's `cloudflared/` has no `cert.pem`; the agent then runs the tunnel's `create` (or reuses the existing tunnel's credentials JSON copied into `cloudflared/`), `route dns -f margin-dev devmargin.krishnanelloore.com`, and `provision-guest.sh dev --tunnel-uuid <UUID>`, which writes `cloudflared/config.yml` from `deploy/cloudflared/config.yml` and starts the `margin-cloudflared` unit. **[owner]** Cloudflare Access policy on `devmargin.krishnanelloore.com`.
6. **[owner, GitHub]** The two OAuth apps (callbacks `https://devmargin.krishnanelloore.com/auth/github/callback`, `https://margin.krishnanelloore.com/auth/github/callback`).
7. **[owner, GitHub]** The settings in §4 (fork approval, branch rule, Environments `dev` and `prod`, read-only token).
8. Push the deploy branch; `build-service` runs `test` then builds; `deploy-dev` installs the binary on the share and starts it. The first start fails `validate_for_serving` if the config has no client id yet: **[owner]** put the "Margin dev" app's client id in `service.toml` (or run `MARGIN_CONFIG=/mnt/shared/margin/service.toml /mnt/shared/margin/bin/margin-service setup` in the guest) and its secret in the `dev` Environment's secrets, then re-run the deploy-dev job. Check `margin-service` answers inside the guest: `curl http://127.0.0.1:8787/api/version`.
9. **[owner]** Open the desktop app, pick **Dev**, sign in with GitHub, enter the model keys in Settings, download the embedding model. Run the §1 embedding measurement and watch `rss_bytes`.
10. **[owner]** Release a dev snapshot (Actions → promote-release, §4a) and merge its PR with a merge commit; `build-service` and `deploy-prod` run; repeat steps 8–9 on prod (environment `prod`, the "Margin" app).
11. **Recreation drill**: restart `margin-dev` from Orchard (a fresh clone). Then `provision-guest.sh dev --rekey` and the step-4 tool; the library, secrets, devices and backups are all still there.
12. Decide Q9 (prod data): if prod takes the owner's library from his old Mac install, stop that service and copy its data folder aside, copy the backup's `data/` to `~/MarginVMs/margin-prod/data` with prod stopped, start, point the app at **Prod**. Then a restore drill: `restore-into-dev.sh`. **Monthly restore drill of dev** (phase 5): run the `restore-drill` workflow (or `deploy/scripts/restore-drill.sh dev` at the host's shell), open the GitHub URL it prints to sign the drill in, and read `backups/drills/<date-time>.txt`; the first run also passes `--hard-stop`. A failed drill is investigated before the next prod deploy. After a provisioning that changes `deploy/systemd/margin-backup.service`, re-run `provision-guest.sh <env>` so the nightly unit gets `--check`.
13. Weekly copy of `~/MarginVMs/margin-prod/backups` to an external disk **[owner: which disk]**: with the disk connected, `deploy/scripts/offsite-backup.sh --install /Volumes/<disk>/MarginBackups` (add `--env all` for dev's too) installs the weekly LaunchAgent; `offsite-backup.sh /Volumes/<disk>/MarginBackups` copies now; `--uninstall` stops it. A week the disk isn't connected fails in the log and copies everything the next time. (`tart export` is no longer needed for data, since the VM holds none.)

## 8. What changes in the repo (area E-deploy)

- `deploy/scripts/{common.sh,provision-guest.sh,deploy.sh,backup-margin.sh,restore-into-dev.sh}` (`backup-margin.sh` moved from `scripts/`; `common.sh` holds the hosts, the share's path and the persistence checks), `deploy/systemd/{margin-service,margin-cloudflared,margin-backup}.service` and `margin-backup.timer` (templates with `__PERSIST__`; they replaced the first build's launchd plists), `deploy/cloudflared/config.yml` (template with `<HOST>`, `<UUID>`, `<CREDS_DIR>`), `.github/workflows/{test,build-service,deploy-dev,promote-prod,apps}.yml`.
- `crates/service/src/config.rs`: `environment` (`"dev" | "prod"`, default `"prod"`), validated, asked first by `setup`. `crates/service/src/version.rs`: `GET /api/version` → `{ version, sha (MARGIN_BUILD_SHA at build time), environment, started_at, rss_bytes, physical_memory_bytes }`, public like `/health`; the low-memory start-up warning. One route line each in `margin_shared::routes` (`VERSION`) and `server.rs`. `crates/service/src/restore.rs` and `margin-service scrub-restored`; `margin-service version`.
- `crates/app/src/config.rs`: `PINNED_SERVICE_URL` from `option_env!("MARGIN_DEFAULT_SERVICE_URL")` (`None` in a developer build) and `LOCAL_SERVICE_URL`; `ui/signin.rs`: sign-in only for a pinned build, the address field for a developer build (§10; the Prod / Dev / Custom picker was removed the same day).
- `crates/evals`: `--service-url` required for `run`, `export` and `report --service`, refusing the prod host and any non-dev `/api/version`; `service_url` left `evals/runner.toml`.
- `docs/service-setup.md`: a note that the VMs follow this plan. `docs/HANDOFF.md`: an E row.
- No change to data layout inside `data_dir`, secret names, auth, or the wire contract beyond the one route.

## 9. Owner questions, with recommendations

Answered by the setup: 1 (domain: `krishnanelloore.com`, single-level subdomains), 4 (two VMs), 5 (sizes, §1).

2. **Which branch deploys to dev?** Recommend `claude/agent-build` now (every phase merges there); switch to `main` once `main` is caught up (still at `dbd08ff`). The workflows name the branch in `build-service.yml`, `deploy-dev.yml` and `test.yml`'s `branches-ignore`.
3. **What promotes to prod?** Built: the exact `deployed-dev-<sha>` artifact, by deploy-dev run id, approved in the `prod` Environment.
6. **Cloudflare Access on dev** (GitHub identity, owner only)? Recommend yes; the eval runner then needs an Access service token (read by `margin-eval`, not built) or runs inside the dev VM.
7. **App builds.** Desktop `.dmg` and Android APK from CI now; iOS TestFlight needs the Apple Developer account: now or after phase 5? Recommend after phase 5.
8. **Signing identity.** None for the service: it is a Linux binary now (the repository secrets `MARGIN_SIGNING_P12_*` are no longer used and can be deleted). Developer ID later for the desktop app and TestFlight.
9. **Does prod hold the owner's real library now?** Recommend not yet: after two weeks of dev, the §1 measurement, the recreation drill (runbook 11) and the restore drill pass, move it (runbook 12) and retire the Mac's service.
10. **Prod → dev restores**: on demand only (built that way), or nightly?
11. **Who else may build?** Built for: only the owner pushes the deploy branch; forks never run on the runner (§4 settings).
12. **Secrets across recreation**: answered by the Linux guests: `secrets.toml` on the share (0600) survives recreation; the deploy jobs write it from the GitHub Environments.
13. **Dev memory**: keep 4 GB and watch `rss_bytes`, or move dev to 6 GB (prod to 6 GB)? Recommend measuring during the first real ingest before changing.

## 10. One service per build (decided 2026-10-14)

**The app is pinned to its service at build time; only a developer build asks for an address.** The owner's question, on seeing "Margin Dev" open with a Prod / Dev / Custom picker: why pick, when the build already knows? Recommended and built:

- `apps.yml` bakes `MARGIN_DEFAULT_SERVICE_URL` into each release build (`config::PINNED_SERVICE_URL`, an `option_env!`, so iOS and Android builds follow the same rule: whatever the build environment says). "Margin" is prod's app and talks only to `margin.krishnanelloore.com`; "Margin Dev" is dev's and talks only to `devmargin.`. Their first screen is "Sign in to Margin [Dev]" with the service's host named under it and one button, "Sign in with GitHub". No picker, no Custom: a prod app that could be pointed at dev (or at a stranger's service) is a mistake waiting to happen, and a dev app that could be pointed at prod is how test data lands in the real library. Settings → Service shows the address read-only.
- A developer build (`dx serve`, `cargo run`, no `MARGIN_DEFAULT_SERVICE_URL`) has no pinned service, so it keeps an address field, starting on the local service (`http://localhost:8787`; `10.0.2.2:8787` from the Android emulator). That is the only way to reach a service running locally (`cargo run -p margin-service`, or the Linux VM), and it answers the owner's "if the service runs locally, the app is built locally too": yes, and the built app is the one that can reach it. Settings → Service keeps Change… there.
- Why not "dev has Custom, prod doesn't": the dev app is what testers install from the `dev-build` release; it should behave like prod, pinned. Reaching other services is a developer's need, so it follows the developer build, not the dev environment.
- Tokens and settings stay per URL (`secrets.rs`, one Keychain entry per service; `app.json` per app name). An older install of a pinned build that had saved another address (say a localhost Custom) is not migrated: the build now reads its own service's token, so it shows sign-in once, and the next sign-in overwrites the saved address. The old token stays in the Keychain unused (`margin-app` never deletes another service's entry).
- The prod and dev hosts no longer live in the app (`PROD_SERVICE_URL`, `DEV_SERVICE_URL`, `PRESETS` and `preset_for` are gone); `apps.yml` is the one place that names them. `examples/render_screens.rs` renders either shape (`RENDER_PINNED`).
