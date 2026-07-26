# S5 — `codex app-server` handshake

Risk-register item **S5** (docs/plan.md §20, background in §3).

> Spawn `codex` over stdio, `initialize`, `account/read`, run one thread. Does
> published protocol 0.63.0 deserialise today's binary? Minimum version floor?

## What this measures

A throwaway NDJSON client that talks to `codex app-server --stdio` and answers
four things:

1. **Does the handshake succeed at all**, and how long does it take?
2. **Is there a protocol version to floor against?** The binary looks for
   `protocolVersion` / `protocol_version` / `version` / `schemaVersion` in the
   `initialize` result and reports `ABSENT` if none is present.
3. **How permissive must aibo's codec be?** Six wire-shape probes (see below).
4. **Does the thread/turn half work end to end** (`--thread`, off by default
   because it spends real quota).

## Running it

```sh
cd spikes/s5_appserver
cargo run                 # handshake + account/read + probes, costs nothing
cargo run -- --thread     # additionally runs ONE real turn (spends quota)
cargo run -- --json       # NDJSON output instead of prose
CODEX_BIN=/path/to/codex cargo run
```

This is a **standalone cargo workspace** (`[workspace]` in its `Cargo.toml`), so
it builds without being a member of the root workspace.

## How to read the result

Every line is `key<spaces>value`. The lines that decide something:

| Key | Meaning |
|---|---|
| `initialize.ok` | `false` here means nothing else in the report matters. |
| `initialize.protocol_version_field` | `ABSENT` ⇒ **a numeric version floor is not obtainable from the handshake.** aibo must gate on `codex --version` and parse permissively. This is the single most important line. |
| `initialize.userAgent` | The server echoes your `clientInfo.name` *and* its own CLI version into the UA string — this is the only version signal on the wire. |
| `probe.*` | `PASS`/`FAIL`. A `FAIL` on `unknown_param_field_ignored` or `connection_survives_error` forces a much more defensive transport in `aibo-agent`. |
| `thread.first_delta_ms` / `thread.turn_total_ms` | Latency of an app-server turn — the §3a evidence that app-server is not a 250 ms autocomplete path. |

## Measured on this machine — 2026-07-26, macOS 26.5.1 (arm64), `codex-cli 0.145.0`

```
codex.version                          codex-cli 0.145.0
spawn.ok                               true
initialize.ok                          true
initialize.latency_ms                  695
initialize.result                      {"codexHome":"/Users/ryuichi/.codex","platformFamily":"unix",
                                        "platformOs":"macos","userAgent":"aibo-spike-s5/0.145.0
                                        (Mac OS 26.5.1; arm64) ghostty/1.3.1 (aibo-spike-s5; 0.1.0)"}
initialize.protocol_version_field      ABSENT
account_read.ok                        true
account_read.result                    {"account":{"email":"<redacted>","planType":"pro",
                                        "type":"chatgpt"},"requiresOpenaiAuth":true}
rate_limits.ok                         true
rate_limits.keys                       rateLimitResetCredits,rateLimits,rateLimitsByLimitId
probe.strict_jsonrpc_field_accepted    PASS
probe.unknown_param_field_ignored      PASS
probe.unknown_method_is_clean_error    PASS   (code -32600, "Invalid request: unknown variant ...")
probe.connection_survives_error        PASS
probe.string_request_id_accepted       PASS
probe.double_initialize                rejected: {"code":-32600,"message":"Already initialized"}
```

With `--thread`:

```
thread.id                    019f9ba1-60c4-7512-9b03-8f6b68b5bcc8
thread.model                 gpt-5.6-sol
thread.turn_total_ms         4837
thread.first_delta_ms        4676
thread.answer                OK
thread.notification_methods  remoteControl/status/changed, thread/started,
                             mcpServer/startupStatus/updated, thread/status/changed,
                             turn/started, skills/changed, item/started, item/completed,
                             item/agentMessage/delta, thread/tokenUsage/updated,
                             account/rateLimits/updated, turn/completed
```

## Findings — what this changes in the plan

**S5 passes.** Handshake, `account/read`, `account/rateLimits/read`, and a full
`thread/start` → `turn/start` → `turn/completed` cycle all work against
`codex-cli 0.145.0` on macOS.

1. **There is no protocol version on the wire.** `InitializeResponse` in 0.145.0
   is `{ userAgent, codexHome, platformFamily, platformOs }` — four fields, none
   of them a version. The plan's "does published protocol 0.63.0 deserialise
   today's binary / minimum version floor?" is therefore the wrong shape of
   question: **you cannot negotiate or floor a version.** aibo's options are
   (a) shell out to `codex --version` and compare, and/or (b) parse permissively
   and fail on the specific missing field. §3's "generate the schema from the
   installed binary" guidance is confirmed as the only reliable route.
2. **`codex app-server generate-json-schema --out <DIR>` works** and emits 39
   top-level schemas plus `v1/` (2 files) and `v2/` (234 files). This is a
   build-time schema-generation path that actually exists today — use it.
   Caveat: the emitted `ClientRequest.json` lists **89** methods, while the
   runtime error message for an unknown method enumerates **~125**. The
   generated schema is a *subset* (experimental/remote-control/process methods
   are omitted), so generating alone is not enough for the experimental surface.
3. **The `-32600` "unknown variant" error is a gift.** It enumerates every
   method the running binary accepts. aibo can send one deliberately-bogus
   method at startup and get the server's full method list for free — a cheaper
   capability probe than schema generation.
4. **The codec can be lenient in both directions.** Sending `"jsonrpc":"2.0"` is
   accepted (so a strict JSON-RPC client library will not be rejected outbound —
   but note the server never *sends* it, so a strict codec still fails on
   inbound frames). Unknown keys in `params` are ignored. String request ids
   work. The connection survives protocol errors.
5. **`initialize` is once-only** — a second one returns `-32600 "Already
   initialized"`. Reconnect logic must spawn a fresh child, not re-handshake.
6. **`initialized` is a real notification** and is the only client notification
   in the protocol.
7. **Rate limits are indeed a separate channel** (§3 confirmed): `account/read`
   returns only `{account, requiresOpenaiAuth}`, while
   `account/rateLimits/read` returns `rateLimits`, `rateLimitsByLimitId`, and
   `rateLimitResetCredits`. Note the plan says the field is `requires_openai_auth`;
   the wire name is camelCase **`requiresOpenaiAuth`**.
8. **`attestation/generate` is a real `ServerRequest`, and it is opt-in.**
   `InitializeCapabilities` has `requestAttestation: bool` (default `false`) —
   "Opt into `attestation/generate` requests for upstream `x-oai-attestation`".
   This is directly load-bearing for **S6**: the app-server route to a
   ChatGPT-subscription turn does *not* need aibo to produce an attestation
   unless it opts in, which strengthens §3a outcome 2 as a fallback.
   There is also a `ChatgptAuthTokensRefresh` server request
   (`account/chatgptAuthTokens/refresh`), i.e. an external-auth mode where the
   *client* owns the tokens — worth a look before committing to device-code.
9. **Turn latency: ~4.7 s to first delta** for a trivial prompt on `gpt-5.6-sol`
   with default settings. §3a's "app-server is not the 250 ms autocomplete path"
   is confirmed by roughly 20×.
10. **Thread defaults observed**: `thread/start` with
    `{cwd, sandbox:"read-only", approvalPolicy:"never", ephemeral:true}` is
    accepted and selects model `gpt-5.6-sol`.

### Cross-check for whoever vendors the protocol types

The generated schemas are the source of truth; a copy can be regenerated any
time with `codex app-server generate-json-schema --out <DIR>`. Names verified
against 0.145.0 that differ from, or are absent in, the plan's §3 prose:

- `account/read` result key is `requiresOpenaiAuth`, not `requires_openai_auth`.
- `thread/start` params are camelCase (`approvalPolicy`, `ephemeral`,
  `threadSource`, `sessionStartSource`); `sandbox` takes
  `"read-only" | "workspace-write" | "danger-full-access"`.
- `turn/start` requires `{threadId, input}` where `input` is an array of
  `{"type":"text","text":...}` / `{"type":"image","url":...}`.
- Agent text streams as `item/agentMessage/delta` with `params.delta`.
