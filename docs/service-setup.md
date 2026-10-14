# Margin service setup

This is how to run `margin-service` on your Mac and reach it at
`https://margin.krishnanelloore.com` through a Cloudflare Tunnel. The service only
listens on `127.0.0.1:8787` and no ports are opened. `cloudflared` connects out to
Cloudflare and forwards requests to it.

Steps marked **[your GitHub account]** or **[your Cloudflare account]** have to be
done by you, signed in to that account. Everything else is a command on the Mac.

**Dev and prod VMs on the Mac mini** follow the same steps, automated and with two
differences: everything durable (`service.toml`, `data/`, backups, the tunnel's
credentials, the binary) lives on a host folder shared into the VM, because Orchard
re-clones a VM's disk whenever it restarts it; and `service.toml` names its
`environment` (`dev` or `prod`), which `margin-service setup` asks first. See
`docs/plan-deploy.md` (§5 for what persists, §7 for the runbook) and `deploy/`. A config
written before that has no `environment` and counts as `prod`.

---

## 1. Build and install the binary

From the repository:

```sh
cd margin
cargo build --release -p margin-service
mkdir -p ~/.local/bin
cp target/release/margin-service ~/.local/bin/
```

The rest of this guide assumes the binary is at `~/.local/bin/margin-service`.

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
~/.local/bin/margin-service setup
```

It asks for:

| Prompt | Answer |
| --- | --- |
| Public URL | Press Return to keep `https://margin.krishnanelloore.com` |
| GitHub OAuth client ID | The Client ID from step 2 |
| GitHub OAuth client secret | The secret from step 2. Typing is hidden. |
| Allowed GitHub login | Press Return to keep `Nkr1shna` |

Setup stores the client secret in your login Keychain as service
`com.krishnanelloore.margin`, account `github-client-secret`. It writes everything else to
`~/Library/Application Support/Margin/service.toml`. Set `MARGIN_CONFIG` to use a
different path.

The finished config looks like this. You can edit it by hand later.

```toml
public_url = "https://margin.krishnanelloore.com"
bind = "127.0.0.1:8787"
data_dir = "/Users/krishnanelloore/Library/Application Support/Margin/data"
github_client_id = "Ov23li…"
allowed_github_login = "Nkr1shna"
github_api_url = "https://api.github.com"
github_oauth_url = "https://github.com"
```

The database lives in `data_dir/db` and rendered videos in `data_dir/videos/<id>/`. To
back up Margin, copy `data_dir` while the service is stopped.

Do a quick test in the foreground:

```sh
~/.local/bin/margin-service serve
# in another terminal:
curl http://127.0.0.1:8787/health      # → ok
```

On the first run the service creates a `media-signing-key` in the Keychain. macOS may ask
whether `margin-service` can use the Keychain. Choose **Always Allow**. Stop the service
with Ctrl-C.

## 4. Install cloudflared

```sh
brew install cloudflared
```

## 5. Log in to Cloudflare **[your Cloudflare account]**

```sh
cloudflared tunnel login
```

A browser window opens. Pick the `krishnanelloore.com` zone, which has to be on your
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
credentials-file: /Users/krishnanelloore/.cloudflared/<UUID>.json

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

## 9. Run cloudflared as a service

To start the tunnel at login with `~/.cloudflared/config.yml`:

```sh
cloudflared service install
```

This installs a LaunchAgent for your user. Check it with
`launchctl list | grep cloudflared`. Its logs are in `~/Library/Logs/`. If your
cloudflared version names the subcommand differently, `cloudflared service --help`
shows the current form. `sudo cloudflared service install` would run it at boot as a
LaunchDaemon instead, and that reads its config from `/etc/cloudflared/`.

## 10. Run margin-service at login with launchd

The service runs as a **LaunchAgent**, not a LaunchDaemon, because it needs your login
Keychain, and that is only unlocked in your session.

Create `~/Library/LaunchAgents/com.krishnanelloore.margin.service.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.krishnanelloore.margin.service</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/krishnanelloore/.local/bin/margin-service</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <!-- So the video pipeline finds manimgx and fluidaudiocli. -->
    <key>PATH</key>
    <string>/Users/krishnanelloore/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>/Users/krishnanelloore/Library/Logs/Margin/service.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/krishnanelloore/Library/Logs/Margin/service.log</string>
</dict>
</plist>
```

Load it:

```sh
mkdir -p ~/Library/Logs/Margin
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.krishnanelloore.margin.service.plist
launchctl print gui/$(id -u)/com.krishnanelloore.margin.service | grep state   # → running
tail -f ~/Library/Logs/Margin/service.log
```

To restart after installing a new build:

```sh
cp target/release/margin-service ~/.local/bin/
launchctl kickstart -k gui/$(id -u)/com.krishnanelloore.margin.service
```

A rebuilt binary has a new code signature, so macOS may ask about Keychain access
again. Choose **Always Allow**.

To stop it and keep it from starting at login:

```sh
launchctl bootout gui/$(id -u)/com.krishnanelloore.margin.service
```

On SIGTERM, which is what `launchctl bootout` sends, the service closes the apps'
live sockets and finishes the requests already in progress. If generation jobs were
running, they show up after the next start as *"Interrupted when the service
stopped."*. Lessons and videos that were still queued start again on their own.

## 11. Finish in the app

1. Open Margin, keep the service URL `https://margin.krishnanelloore.com`, and choose
   **Sign in with GitHub** **[your GitHub account]**. Approve the OAuth app in the
   browser. The page says *"Signed in to Margin. You can return to the app."*
2. In **Settings → AI provider**, enter your OpenAI API key. The service stores it in the
   Mac's Keychain as account `openai-api-key` and never sends it back. Enter the model
   name too: there is no default, and Ask, Regenerate and Make video return `no_model`
   until it is set. Use any model your key can call through the Responses API. The base
   URL defaults to `https://api.openai.com/v1`.
3. **Settings → Tools** shows whether `manimgx` and `fluidaudiocli` are found on the
   Mac, with install commands to copy.
4. **Settings → Devices** lists every signed-in device. Revoking one takes effect
   at once and closes its live connection.

## Reference

- `margin-service serve [--allow-unsandboxed-render]`: with the flag, AI-written scene
  code renders **without** the macOS sandbox. It is meant for development and tests
  only. Don't put it in the plist.
- Sign-in and polling are rate-limited per client IP. Behind the tunnel the IP comes from
  Cloudflare's `CF-Connecting-IP` header.
- Logs never contain tokens, API keys, OAuth codes or media signatures. Request lines
  are logged without query strings, at `RUST_LOG=margin_service=debug`.
- Keychain items, all under service `com.krishnanelloore.margin`:
  `github-client-secret`, `openai-api-key` and `media-signing-key`. Deleting
  `media-signing-key` makes every media link already handed out invalid. The service
  creates a new key at its next start.
