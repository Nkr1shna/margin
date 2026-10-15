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
