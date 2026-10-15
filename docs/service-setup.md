# Margin service setup

This is how to run `margin-service` on a Linux box (Ubuntu 24.04, arm64 or x86_64) and
reach it at `https://margin.krishnanelloore.com` through a Cloudflare Tunnel. The service
only runs on Linux (`docs/plan-linux-service.md`); the apps run on Mac, iPhone and
Android. The service only listens on `127.0.0.1:8787` and no ports are opened.
`cloudflared` connects out to Cloudflare and forwards requests to it.

Steps marked **[your GitHub account]** or **[your Cloudflare account]** have to be
done by you, signed in to that account. Everything else is a command on the box.

**Dev and prod VMs on the Mac mini** follow the same steps, automated and with two
differences: everything durable (`service.toml`, `secrets.toml`, `data/`, backups, the
tunnel's credentials, the binary) lives on a host folder shared into the VM, because
Orchard re-clones a VM's disk whenever it restarts it; and `service.toml` names its
`environment` (`dev` or `prod`), which `margin-service setup` asks first. See
`docs/plan-deploy.md` (§5 for what persists, §7 for the runbook) and `deploy/`. A config
written before that has no `environment` and counts as `prod`.

---

## 1. Build and install the binary and the tools

The guest image (`deploy/image`) has everything below; on another box:

```sh
sudo apt install tesseract-ocr tesseract-ocr-eng ffmpeg bubblewrap libheif1 libc++1
cd margin
cargo build --release -p margin-service
sudo install -m 0755 target/release/margin-service /usr/local/bin/
```

`pdfium` (PDF text and pages), `manimgx` (video lessons) and `margin-tts` (narration)
are installed the way **Settings → Tools** shows (§9). Bubblewrap needs unprivileged
user namespaces, which Ubuntu 24.04 restricts through AppArmor: the Tools row for
`bwrap` shows the profile to install (`docs/plan-linux-service.md` §5.1).

## 2. Create the GitHub OAuth App **[your GitHub account]**

1. Open <https://github.com/settings/developers> → **OAuth Apps** → **New OAuth App**.
2. Fill in:
   - **Application name:** `Margin`
   - **Homepage URL:** `https://margin.krishnanelloore.com`
   - **Authorization callback URL:** `https://margin.krishnanelloore.com/auth/github/callback`
   - Leave **Enable Device Flow** unchecked.
3. **Register application**. Copy the **Client ID**.
4. Click **Generate a new client secret** and copy the secret. GitHub shows it only once.

The service asks GitHub only for the `read:user` scope. It uses that to read your
login and checks it against `allowed_github_login`.

## 3. Run `margin-service setup`

```sh
sudo mkdir -p /etc/margin /var/lib/margin && sudo chown "$USER" /etc/margin /var/lib/margin
margin-service setup
```

It asks for:

| Prompt | Answer |
| --- | --- |
| Environment | `prod` (or `dev`) |
| Public URL | Press Return to keep `https://margin.krishnanelloore.com` |
| GitHub OAuth client ID | The Client ID from step 2 |
| GitHub OAuth client secret | The secret from step 2. Typing is hidden. |
| Allowed GitHub login | Press Return to keep `Nkr1shna` |

Setup stores the client secret in the secrets file, `/var/lib/margin/secrets.toml`
(0600, beside the data folder so a copy of the data folder never copies keys; set
`secrets_file` to put it elsewhere), as `github-client-secret`. It writes everything
else to `/etc/margin/service.toml`. Set `MARGIN_CONFIG` to use a different path.

The finished config looks like this. You can edit it by hand later.

```toml
public_url = "https://margin.krishnanelloore.com"
bind = "127.0.0.1:8787"
data_dir = "/var/lib/margin/data"
github_client_id = "Ov23li…"
allowed_github_login = "Nkr1shna"
github_api_url = "https://api.github.com"
github_oauth_url = "https://github.com"
environment = "prod"
embedder = "onnx-q8"
secrets = "file"
```

The database lives in `data_dir/db` and rendered videos in `data_dir/videos/<id>/`. To
back up Margin, stop the service and run `deploy/scripts/backup-margin.sh`.

Do a quick test in the foreground:

```sh
margin-service serve
# in another terminal:
curl http://127.0.0.1:8787/health      # → ok
```

On the first run the service creates a `media-signing-key` in the secrets file. Stop
the service with Ctrl-C.

To download the models without the app (the embedding model and the speech model,
into `data_dir/models`): `margin-service download-model`.

## 4. Install cloudflared

From Cloudflare's apt repository (the guest image has it), or the `.deb` from
<https://github.com/cloudflare/cloudflared/releases>.

## 5. Log in to Cloudflare **[your Cloudflare account]**

```sh
cloudflared tunnel login
```

It prints a URL; open it, pick the `krishnanelloore.com` zone, which has to be on your
Cloudflare account. This writes `~/.cloudflared/cert.pem`.

## 6. Create the named tunnel **[your Cloudflare account]**

```sh
cloudflared tunnel create margin
```

Note the tunnel UUID it prints. It also writes the tunnel's credentials to
`~/.cloudflared/<UUID>.json`.

## 7. Route DNS to the tunnel **[your Cloudflare account]**

```sh
cloudflared tunnel route dns margin margin.krishnanelloore.com
```

This creates the proxied CNAME `margin.krishnanelloore.com → <UUID>.cfargotunnel.com`.

## 8. Write `~/.cloudflared/config.yml`

```yaml
tunnel: <UUID>
credentials-file: /home/<you>/.cloudflared/<UUID>.json

ingress:
  - hostname: margin.krishnanelloore.com
    service: http://127.0.0.1:8787
  - service: http_status:404
```

Replace `<UUID>` in both places. Cloudflare passes WebSockets through by default, so
the app's live `/api/events` socket needs no extra settings.

Test it with the service running from step 3:

```sh
cloudflared tunnel run margin
# in another terminal:
curl https://margin.krishnanelloore.com/health   # → ok
```

## 9. Run both with systemd

The units in `deploy/systemd` are the ones the VMs use (`provision-guest.sh` fills in
`__PERSIST__`, the folder holding `service.toml`, `bin/` and the data, and
`__MEMORY_MAX__`). On a box of your own, adapt `margin-service.service`:
`Environment=MARGIN_CONFIG=/etc/margin/service.toml`,
`ExecStart=/usr/local/bin/margin-service serve`, `ReadWritePaths=/var/lib/margin`, and
`User=` the user that owns `/var/lib/margin`. Keep the unit's note: no
`RestrictNamespaces=`, `SystemCallFilter=` or `PrivateUsers=`, because the scene sandbox
(bubblewrap) creates namespaces. `margin-cloudflared.service` runs
`cloudflared --config ~/.cloudflared/config.yml tunnel run`.

```sh
sudo cp margin-service.service margin-cloudflared.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now margin-service margin-cloudflared
journalctl -u margin-service -f
```

To restart after installing a new build:

```sh
sudo install -m 0755 target/release/margin-service /usr/local/bin/
sudo systemctl restart margin-service
```

On SIGTERM, which is what `systemctl stop` sends, the service closes the apps' live
sockets and finishes the requests already in progress. If generation jobs were
running, they show up after the next start as *"Interrupted when the service
stopped."*. Lessons and videos that were still queued start again on their own.

## 10. Finish in the app

1. Open Margin, keep the service URL `https://margin.krishnanelloore.com`, and choose
   **Sign in with GitHub** **[your GitHub account]**. Approve the OAuth app in the
   browser. The page says *"Signed in to Margin. You can return to the app."*
2. In **Settings → AI provider**, enter your OpenAI API key (or sign in with ChatGPT,
   or a Claude key). The service stores it in its secrets file and never sends it back.
3. **Settings → Sources** downloads the embedding model and the speech model.
4. **Settings → Tools** shows whether `manimgx`, `margin-tts`, `tesseract`, `ffmpeg`,
   `bwrap` and `pdfium` are found on the service, with install commands to copy.
5. **Settings → Devices** lists every signed-in device. Revoking one takes effect
   at once and closes its live connection.

## Reference

- `margin-service serve [--allow-unsandboxed-render]`: with the flag, AI-written scene
  code renders **without** the bubblewrap sandbox. It is meant for development and tests
  only. Don't put it in the unit.
- Sign-in and polling are rate-limited per client IP. Behind the tunnel the IP comes from
  Cloudflare's `CF-Connecting-IP` header.
- Logs never contain tokens, API keys, OAuth codes or media signatures. Request lines
  are logged without query strings, at `RUST_LOG=margin_service=debug`.
- Secrets in the secrets file (`name = "value"` lines): `github-client-secret`,
  `openai-api-key`, `anthropic-api-key`, `chatgpt-tokens` and `media-signing-key`.
  Deleting `media-signing-key` makes every media link already handed out invalid. The
  service creates a new key at its next start.
