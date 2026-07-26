# S9 — Complete quality + eval harness

> Chat-instruct vs FIM vs local small model for Complete (§5), measured on ~50
> real fixtures in Japanese and English. Pass rate and TTFT per candidate.
> **If it fails: pick the best available; the harness matters more than the answer.**
> — §20

Read that fallback line again, because it sets the bar for this spike: **the
deliverable is the harness, not the number.** §5 is blunt about why —

> Every threshold above is an unfalsifiable guess without a way to measure. […]
> Without it, prompt work is vibes and every regression is invisible.

So a run that produces a mediocre pass rate but a repeatable table is a
successful spike. A run that produces a great number you cannot reproduce next
week is not.

Related plan sections: §5 (prompt specs, the language rule, untrusted captured
content), §18 tier 1–2 (what of this graduates into CI), §20 (the S9 row).

## What this measures, and what it cannot

Scored automatically, per §5's "expected properties rather than exact strings":

| Property | Applies to | Fires when |
|---|---|---|
| `non_empty` | all | the reply is blank |
| `no_preamble` | all | "Sure," / "Here's" / 「承知しました」 / a leading code fence |
| `no_prefix_repetition` | Complete | the reply restates >12 graphemes of the prefix tail, or a whole sentence from it |
| `no_suffix_duplication` | Complete w/ suffix | the reply duplicates the text after the caret |
| `language_match` | all | the reply's script does not match the fixture's declared language |
| `whitespace_preserved` | Transform | leading/trailing whitespace of the selection was changed |
| `no_added_code_fence` | Transform | the reply added a fence the input did not have |
| `length_within_bounds` | Complete, Transform | over the fixture's cap (default 400 graphemes for Complete, 4× the selection for Transform) |
| `ends_at_sentence_boundary` | Complete | the reply trails off mid-clause |

**Not scored, and you must read for them anyway:**

- **Register.** `cmp-ja-mail-keigo` in plain form passes `language_match` and is
  still wrong. Japanese politeness level is the single most likely quality
  failure this product will ship with, and no property here catches it.
- **Prompt injection.** `tr-en-prompt-injection` and `ask-ja-attachment-injection`
  are pass/fail *by eye*. §5 requires captured content be fenced and labelled
  untrusted; these two fixtures are how you find out whether that worked.
- **Whether the continuation is any good.** Every property can hold on a reply
  that is fluent and useless.

The `looks_japanese` heuristic is script-based and deliberately crude. It is not
a language detector and `cmp-ja-kanji-only-head` exists specifically to find its
edge. If it misclassifies, record that as a harness limitation, not a model
failure.

## What the operator does

```sh
cd spikes/s9_eval

# 0. See the corpus and read one assembled prompt before trusting any number.
cargo run -- list
cargo run -- show --id cmp-en-mail-midsentence
cargo run -- show --id cmp-en-mail-midsentence --prompt-version complete/v2-terse

# 1. Sweep one candidate at a time. One JSONL file per candidate.
export OPENAI_API_KEY=...
cargo run -- run \
  --base-url https://api.openai.com/v1 \
  --model <model-id> \
  --api-key-env OPENAI_API_KEY \
  --out out/gpt.jsonl

# a local small model — no key needed
cargo run -- run \
  --base-url http://localhost:11434/v1 \
  --model qwen2.5:7b \
  --out out/local.jsonl

# the terse prompt against the same model: this is the prompt A/B
cargo run -- run \
  --base-url https://api.openai.com/v1 --model <model-id> \
  --api-key-env OPENAI_API_KEY \
  --surface complete --prompt-version complete/v2-terse \
  --out out/gpt-terse.jsonl

# 2. Score them all together.
cargo run -- check --recorded out/gpt.jsonl --recorded out/local.jsonl --recorded out/gpt-terse.jsonl
```

`run` and `check` are separate on purpose. Change a property assertion, re-run
`check` on the same JSONL, and you get the new score for free — no network, no
spend. That is the whole reason the outputs are recorded rather than scored in
flight.

Candidates are labelled `<model> @ <prompt-version>`, so the same model under two
prompts is two rows in the table.

`--api-key-env` takes the *name* of an environment variable, never the key. A key
on a command line lands in the shell history.

## Growing the fixture set — do this first

`fixtures/` ships **25 cases**. §5 asks for **~50 per surface**, and `list`
prints a warning until you get there. The seed set is a *shape*: it enumerates
the failure modes the plan names (mid-sentence caret, mid-word caret, keigo
register, ideographic whitespace, injection, emoji graphemes) with one or two
instances each.

> A harness scored on invented text measures the inventor, not the model.

Capture real cases from your own daily use — real Slack drafts, real mail, real
editor buffers — and add JSON files to `fixtures/`. Any `*.json` file holding an
array of fixtures is picked up; ids must be unique across the whole directory
because they are the join key with recorded outputs.

The field that matters most and is easiest to forget is `suffix`. §5:

> completing into the middle of existing text without knowing what follows
> produces duplicates, and this is the single most common autocomplete failure.

A corpus where every fixture has an empty `suffix` makes
`no_suffix_duplication` vacuous and hides the failure the product will actually
have.

## Limits of the transport

`src/live.rs` speaks one wire format: `POST {base_url}/chat/completions` with
`stream: true`. That covers every §10 OpenAI-compatible provider and Ollama, and
it deliberately does **not** grow a provider matrix — that belongs in
`aibo-provider`.

Consequences to state in the writeup:

- **The FIM arm of the spike is not implemented.** §5 leaves open whether
  Complete should use a fill-in-the-middle endpoint; measuring that needs a
  second transport (`prompt`/`suffix` fields, not chat messages). What is
  measured here is the *instruct-a-chat-model* option only, against a local
  small model. Marked `SPIKE: S9` in `src/live.rs`.
- **TTFT is measured to the first non-empty content delta**, over a pooled
  connection, from your machine on your network. It is comparable *between
  candidates in the same run*, and not comparable to §15's budget numbers or to
  someone else's benchmark.

## What to record — the go/no-go note

```
S9 — Complete quality + eval harness
Date / network / machine:
Fixture count at time of run:      complete __ / transform __ / ask __
Prompt versions swept:

Report table (paste the output of `check`)

Per-candidate judgement — read the failure listing, do not stop at the table
  candidate:
    clean rate:               __%
    TTFT p50 / p90:           __ / __ ms
    register correct (ja):    yes | mostly | no      <- not machine-scored
    resisted injection:       yes | no               <- not machine-scored
    worst failure mode:

Harness limitations hit this run
  looks_japanese misclassified:      which fixtures:
  a property fired on a good reply:  which, and why:
  a property missed a bad reply:     which, and why:

Decision (§5)
  Complete uses:  [ ] chat-instruct   [ ] FIM endpoint   [ ] local small model
  Prompt version promoted to aibo-core/prompts/complete.md:
  Properties promoted into CI (§18 tier 1):
```

The last two lines are the actual output of this spike. `src/properties.rs` is
pure, has no I/O, and is unit-tested — it is meant to be **copied** into
`aibo-core` (a copy, not a dependency: a spike must never be linked into the
product build). The prompt version that wins becomes the first
`aibo-core/prompts/*.md`.

## Not in scope

- Token accounting and cost (§14) — TTFT is measured, spend is not.
- Retrieval, tools, multi-turn. Every fixture is one turn.
- Any provider-specific wire format. That is `aibo-provider`'s job.
