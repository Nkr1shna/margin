## "Sign in with ChatGPT" spike — results (D11)

Run by the owner on 2026-10-14 on the service Mac with their own ChatGPT plan, from a throwaway binary in a worktree that is not merged (`docs/agent-providers-brief.md` D11). It wrote nothing to Margin's data folder and touched no running service. No token, id or email is recorded here.

**Outcome: GO.** Steps 1–4 passed, so the ChatGPT provider is built as designed: a normal, selectable provider (no capability gate), with web search on. Step 5 (the per-app usage cap) was not exercised: the mapping of `subscription_sharing_usage_limit_exceeded` (HTTP 429, or a mid-stream `response.failed`) to `plan_limit_reached` is built as the docs describe but is **untested against the live API**.

Setup: auth base `https://auth.openai.com`, API base and `resource` `https://api.openai.com/v1`, scope `openid profile email offline_access resource.invoke chatgpt.tokens.use.direct`, loopback callback `http://127.0.0.1:1455/auth/callback`, model `gpt-6.1-sol` for every call.

### Step 1 — registration, loopback + PKCE, token exchange

| Check | Result |
|---|---|
| Authorization (`/api/accounts/authorize`, PKCE S256, `state`/`nonce`, `ext_agent_host_id`; this run re-authorized with the issued `client_id`) | accepted; callback with `code`, `scope`, `state` |
| Token exchange (`/api/accounts/oauth/token`) | HTTP 200: `access_token`, `refresh_token`, `id_token`, `expires_in` 3600, `earliest_refresh_at`, `scope`, `token_type` Bearer |
| `chatgpt.tokens.use.direct` granted | yes |
| Access token claims | `iss` `https://auth.openai.com`, `aud` `https://api.openai.com/v1`, lifetime 3600 s |
| `id_token` | `iss`, `aud == client_id`, `exp`, `nonce` all check; RS256 with a `kid` found in the JWKS (signature not verified by the spike: the service must) |
| OpenID configuration | `jwks_uri` `/.well-known/jwks.json`, `token_endpoint` as above, and a **`revocation_endpoint`** `/api/accounts/oauth/revoke` (the brief assumed none: sign-out may revoke remotely) |
| `GET /models` with the bearer | HTTP 200, SIWC shape (`models[]`); `visibility == "list"`: `gpt-6.1-sol`, `gpt-6-astra`, `gpt-6-sol`, `gpt-6-luna`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` |

### Step 2 — Responses with the bearer (`store: false`, `stream: true`)

| Case | Result |
|---|---|
| 2a baseline | accepted |
| 2b developer message + `instructions` | accepted |
| 2c `reasoning` + `include: ["reasoning.encrypted_content"]` | accepted; reasoning item with `encrypted_content` |
| 2d strict function tools | accepted; one `function_call` |
| 2e `function_call_output` replayed | accepted |
| 2e2 replayed `reasoning` item + message, then a follow-up | accepted |
| 2f `tool_choice: allowed_tools` (mode `required`, only `get_time`) | accepted and honoured, **but one response returned 128 `get_time` calls** |
| 2g `context_management` compaction (threshold 200 000) | accepted |
| 2h compaction at a low threshold (1 000) | accepted; `compaction` items in the output |
| 2i replayed `compaction` item | accepted |
| 2j `prompt_cache_key` + `parallel_tool_calls: false` | accepted |
| 2k the full agent request (all of the above together) | accepted |

### Step 3 — hosted `web_search`

| Case | Result |
|---|---|
| 3a `web_search` + `include: ["web_search_call.action.sources"]` | accepted; `web_search_call` (action `search`, sources listed) and a message with `url_citation` annotations |
| 3b `web_search_call` (+ reasoning, message) replayed as input | accepted |
| 3c `web_search` alongside strict function tools | accepted |

### Step 4 — refresh with rotation

| Case | Result |
|---|---|
| `grant_type=refresh_token` | HTTP 200; new access token; refresh token rotated; `earliest_refresh_at` present |
| Responses call with the refreshed token | accepted |

### Step 5 — per-app usage cap

Not exercised. Right after the spike, ChatGPT → Settings → Usage → "App limits" listed only other apps, not Margin, so a per-app cap for Margin couldn't be set; it may appear only once usage has been counted against the app. The per-app cap is therefore unverified. Margin must work normally with no cap set, and show `plan_limit_reached` whenever ChatGPT refuses a call for usage.

### What the build carries from this

- **P-ai-openai / P-agent:** with `allowed_tools` in `required` mode one response held 128 parallel calls of the same tool. On ChatGPT the agent sends `parallel_tool_calls: false` (or caps the calls it runs per step) so a step can't fan out like that.
- **P-ai-openai:** `max_output_tokens`, `temperature`, `top_p`, `user`, `previous_response_id` were not tested and stay unsent under the bearer, as the brief says; everything the agent sends today was accepted.
- **P-service:** honour `earliest_refresh_at` (don't refresh before it), refresh on demand under a mutex as designed; sign-out may call the `revocation_endpoint` before deleting the tokens; validate the `id_token` signature against the JWKS (the spike didn't).
- **Everyone:** `plan_limit_reached` is untested (step 5); its handling follows the docs.

## Signing in when the service is remote (2026-10-15)

The spike ran with the browser and the service on one Mac. The service now runs on a headless Linux VM (dev: `devmargin.krishnanelloore.com`, prod later), reached through a Cloudflare tunnel, and the person signs in from their own Mac or phone. The first build had the service listen for the OAuth callback on its own `127.0.0.1:1455`, while the app opened the authorize URL in the device's browser: the redirect landed on the device's loopback, where nothing listened, so sign-in could never finish.

Constraints: the sign-in is the organisation's (admin only, `RequireAdmin`); tokens live only in the service's secret store (API keys and tokens never stay on a device beyond transit); it has to work from the desktop app and from phones.

### What OpenAI allows (checked 2026-10-15)

- **Redirect URIs.** SIWC "Registration and sign-in" (`developers.openai.com/siwc/token-sharing-open-source/sign-in`): the `redirect_uri` must be an HTTP loopback callback on `127.0.0.1` "from initial registration onward" (`http://127.0.0.1:1455/auth/callback` in the example). Later sign-ins may use another port, and only the port may vary: the scheme, host and path stay, with no `localhost`. The page lists no other redirect, no custom scheme and no hosted callback. A redirect to the service's own HTTPS address isn't available to a dynamically registered client.
- **Self-hosted VMs.** The SIWC page for this case (`…/token-sharing-open-source/self-hosted-vms`) says a `127.0.0.1` callback "reaches the computer running the browser, not the remote VM". Its advice is to finish OAuth locally with the same client, then copy the credential file to the VM over SSH, keeping the VM's own `ext_agent_host_id` and letting the VM do the later refreshes.
- **Device code.** No SIWC page documents a device-authorization flow for SIWC clients (`dynamic_agent_client` or an issued `oaiapp_…` id). Codex has one: `codex login --device-auth` (`codex-rs/login/src/device_code_auth.rs` in `openai/codex`, read at `main` 22a3f6d, 2026-09-23). It works like this:
  - It posts `client_id` to `{issuer}/api/accounts/deviceauth/usercode`.
  - The person enters the code at `{issuer}/codex/device` within 15 minutes.
  - It polls `{issuer}/api/accounts/deviceauth/token`, which returns an authorization code with a PKCE verifier the server generated.
  - It exchanges that code with `redirect_uri = {issuer}/deviceauth/callback`.

  That flow runs on Codex's own first-party client id (`app_EMoamEEZ…`, `codex-rs/login/src/auth/manager.rs`), not on the SIWC token-sharing contract. Using Codex's client id would make Margin impersonate Codex. Calling `deviceauth` with Margin's issued client id is undocumented and untested, and could fail or change without notice. Third-party Codex guides also say device-code login has to be switched on in ChatGPT's security settings, or by a workspace admin. That isn't verified against OpenAI's own docs.

### Options considered

| | How | Works from | Tokens | Verdict |
|---|---|---|---|---|
| (b) Device code | The service asks for a user code; the person enters it at an OpenAI page; the service polls | any device, no listener | service only | **Rejected for now.** It isn't part of the SIWC contract for Margin's client (above). Worth a 10-minute spike later: `POST deviceauth/usercode` with the issued `oaiapp_…` client id. If OpenAI documents it for SIWC, it should replace the paste step. |
| SIWC's VM recipe | Run OAuth on a laptop, copy the credentials to the VM | a computer with a shell | pass through the laptop's disk | **Rejected.** It breaks the rule that tokens never sit on a device, and phones can't do it. |
| (a) Desktop listener + code forwarding | The service makes the PKCE pair, `state` and `nonce` and keeps them. The app listens on the device's `127.0.0.1:<port>/auth/callback` during the sign-in and forwards the address (code + state) to the service, which exchanges the code and stores the tokens | desktop (macOS, Linux, Windows) | service only; the code crosses the device but is useless without the verifier (PKCE), single-use, and minutes-lived | **Chosen** for desktop |
| (c) Paste the address | The browser ends on a page that doesn't load, `http://127.0.0.1:1455/auth/callback?code=…&state=…`; the person copies that address into the app, which sends it to the same route | everywhere (phones, a browser on another computer, a desktop whose port is taken) | as (a) | **Chosen** for phones, and as the fallback everywhere |

Phones don't run a listener. On iOS an app in the background is suspended while Safari is in front, so a loopback listener in it isn't reliable. `ASWebAuthenticationSession` and Custom Tabs only take custom-scheme or HTTPS callbacks, which SIWC doesn't allow.

### What was built

- `POST /api/providers/chatgpt/start` takes an optional `ChatGptStartRequest { callback_port }`. The desktop app binds 1455, or a free port when 1455 is taken, which SIWC allows. The route builds the authorize URL with `redirect_uri = http://127.0.0.1:<port>/auth/callback` and keeps the verifier, `state`, `nonce`, redirect and client id in memory on the service. Nothing listens on the service any more.
- `POST /api/providers/chatgpt/complete` takes `ChatGptComplete { callback_url }` and runs these checks:
  - It reads the whole address or only its query. Anything that isn't an `http` loopback `/auth/callback` with a `state` gets 400 `invalid_callback`.
  - It takes the waiting sign-in only when `state` matches. Another sign-in's state gets 400 `invalid_state`, and the real one keeps waiting.
  - It uses each sign-in once: the sign-in moves to "exchanging" and a replay gets 409 `no_sign_in`. A sign-in older than 10 minutes is dropped, verifier included.
  - It exchanges the code with the verifier and validates the `id_token` (JWKS, issuer, audience, expiry, nonce).
  - It rejects a callback whose `client_id` differs from the stored registration.
  - It stores the tokens only while the same sign-in is still current, so a sign-out or a newer sign-in wins.

  The route is admin-only and rate limited per principal (`auth::CHATGPT_COMPLETE_LIMIT`, 20 a minute; start 10). Codes, verifiers and tokens are never logged, and the answer is the account, never a token.
- **App (desktop).** `crate::chatgpt_callback` listens during the sign-in (10 minutes, one callback with the right state; other requests get a short page). It forwards the address and answers the browser tab with "Margin is signed in with ChatGPT" or the reason it failed. It stops when the sign-in ends in any way: finished, cancelled, replaced or out of time.
- **App (every platform).** While waiting, Settings → AI → ChatGPT shows "Open in browser", "Copy link", "Cancel" and the field "The address the browser ended on" with "Finish signing in". On phones the help text explains copying the `http://127.0.0.1:1455/auth/callback?…` address.
- The service keeps its own `ext_agent_host_id`, matching SIWC's VM guidance that the host that refreshes owns the host id.

Tested on fakes only: a fake OAuth server in `crates/service/tests/providers.rs` and the app's `fake.rs`. Live, the owner runs the steps in the PR notes on dev.
