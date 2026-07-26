# S6 — Codex endpoint via device-code auth

> **This is the single highest-value unknown in the plan.** §3a's entire
> "direct HTTPS to `CHATGPT_CODEX_BASE_URL`" design lives or dies on one
> question, and P3 cannot be scheduled until it is answered:
>
> **Does a Responses call succeed with `Authorization` + `ChatGPT-Account-ID`
> and *without* `x-oai-attestation`?**

Related plan sections: §3a (the split and the three outcomes), §3b (wire capture
and the ToS position), §20 (the S6 row).

## Before you start — read this

1. The device flow uses **Codex's own OAuth client id** (§3a). The consent page
   is literally `auth.openai.com/codex/device` and it says *Codex*, not aibo.
   You are deciding whether that posture is shippable, not only whether the code
   works. Write down your answer.
2. §3b's position on terms of service: *not demonstrably a violation; definitely
   not sanctioned.* Anthropic banned the equivalent in Jan 2026. Run this on an
   account you are willing to put at residual risk.
3. `s6-tokens.json` holds a live access token and refresh token. **Delete it
   when you are done.** It is not in a keychain and it is not encrypted.
4. Everything marked `SPIKE: S6` in the source is a guess the spike exists to
   replace — the device-auth body encoding, the request field names, the model
   id, and the Codex-client header set. If the first run 400s, fix the guess
   before you conclude anything about auth.

## What the operator does

```sh
cd spikes/s6_codex_auth

# 1. Device-code login. RFC 8628 says form-encoded; try that first.
cargo run -- login
#    ...if the usercode POST returns 400/415, the body encoding is the problem:
cargo run -- login --encoding json

# 2. Sanity-check the identity you just obtained.
cargo run -- inspect

# 3. The header matrix. This is the actual spike.
cargo run -- probe
```

Step 1 prints a user code and polls. Open the printed URL, enter the code,
approve. The tokens land in `s6-tokens.json`.

Step 3 runs four requests and prints a markdown table plus a verdict mapped onto
§3a's three outcomes. The full report is written to `s6-report.json`.

## The four variants and why each exists

| Variant | Headers sent | What a pass/fail tells you |
|---|---|---|
| `minimal` | `Authorization`, `ChatGPT-Account-ID` | **The go/no-go.** A pass here is §3a outcome 1 and the plan proceeds unchanged. |
| `no_account_id` | `Authorization` only | Whether `ChatGPT-Account-ID` is genuinely mandatory, or whether §3a overstates it. |
| `codex_like` | + `OpenAI-Beta`, `originator`, `session_id`, `User-Agent` | Whether the Codex-client headers are load-bearing. If `minimal` fails and this passes, one of these four headers is the gate — bisect it by hand and record which. |
| `bogus_attestation` | + a deliberately invalid `x-oai-attestation` | Whether the backend **validates** the attestation or merely checks that it is present. These are very different futures: a presence check is a hole that will close, and shipping on it means shipping behind §3b's fallback chain. |

## What to record — the go/no-go note

Copy this into the P0 spike writeup:

```
S6 — Codex endpoint via device-code auth
Date / OS / account plan type:
codex CLI version consulted for the client id:

Device flow
  usercode encoding that worked:            form | json
  usercode response field names:
  token response field names:
  chatgpt_account_id claim location:        top-level | https://api.openai.com/auth | other
  refresh_token issued:                     yes | no
  access token lifetime (expires_in):

Probe matrix (paste the printed table)

Answers
  Succeeds without x-oai-attestation:       YES | NO
  ChatGPT-Account-ID mandatory:             YES | NO
  Which extra headers were load-bearing:
  Invalid attestation accepted:             YES | NO
  TTFT (ms), best variant:                  ____   (compare to Cerebras/Groq, §15)
  Cookie jar needed:                        YES | NO | unknown
  Rejection body names attestation:         YES | NO | n/a

Decision (§3a)
  [ ] Outcome 1 — direct endpoint. P3 keeps the `codex` provider.
  [ ] Outcome 2 — attestation required. Subscription inference falls back to
      `codex app-server` with tools disabled and approvalPolicy "never".
  [ ] Outcome 3 — Codex stays agent-only (the Do surface).

Consent-screen posture (says "Codex", tokens go to aibo): acceptable? YES | NO
```

## Capturing the real wire format

If the probe rejects and you need the true request shape, §3b is explicit about
how — and about one dead end:

- **Use mitmproxy with a trusted CA.** Codex supports a custom CA, so point it
  through the interceptor and drive a real Codex turn.
- **Do not use `codex-responses-api-proxy`.** It is API-key-only (key read from
  stdin under `mlock`) and hardcoded to forward to `https://api.openai.com/v1/responses`;
  it will never see the ChatGPT-subscription path.

Then fix `request_body()` in `src/probe.rs` and re-run.

## Not in scope for this spike

Token refresh. §3a notes aibo takes on the full lifecycle — refresh before
expiry with jitter, `refresh_token_reused` and `refresh_token_invalidated` as
first-class error states — but that is a day of P3 work, not a go/no-go. This
spike only proves the token is usable at all.
