# Repo Guardian

Webhook-driven GitHub PR reviewer: a Rocket server receives `pull_request`
webhooks, checks the PR out locally, has the Claude Code CLI review it, and
posts the result back as a single PR review (approve / request changes /
comment), optionally auto-merging.

## Flow

1. `POST /webhook/gh` — data guard validates `X-Hub-Signature-256` (HMAC-SHA256
   of the raw body) before parsing; the delivery is ACKed with 202 and the
   pipeline runs in a spawned task (GitHub times deliveries out at 10s).
2. `state::StateStore::begin_review` atomically claims the (PR, head sha);
   duplicates/redeliveries are skipped.
3. `repos::resolve` clones/fetches `<repos_path>/<name>` (no owner prefix,
   so hand-made clones are picked up as-is); the PR head
   (`refs/pull/N/head`) is fetched and materialized as a detached worktree
   (`<name>-pr-N`) so the reviewer reads the PR's version of files.
4. `guardian::Guardian::review` runs `claude --print` (via the `claude-code`
   crate) with a JSON schema; returns `{approved, comments[], resolved_previous[]}`.
5. Pipeline resolves fixed comment threads (GraphQL), posts one review with all
   inline comments, records the posted comment ids in state, merges if
   `approved && auto_merge`.

## Module map

- `main.rs` — wiring (`App` state), tracing init, boot-time path validation
- `webhook.rs` — endpoint + signature data guard
- `pipeline.rs` — orchestration of one review round
- `guardian.rs` — the reviewer (prompt, schema, severities, Claude CLI call)
- `github/payload.rs` — webhook payload types; `github/client.rs` — octocrab wrapper
- `repos.rs` — clone/fetch/worktree management
- `state.rs` — persisted per-PR state (idempotency, open comments, caps)
- `config.rs` — `config.toml` (`auto_merge`, `repos_path`, optional caps)

## Decisions

- **Guardian (né Hermes) is in-process**, not an external service: the
  `claude-code` crate runs the CLI as a subprocess with `--json-schema`.
  It's generic over `CommandRunner` so tests inject a fake runner.
- **All comments go up as ONE review** per round, never individual comments.
- **Severities** (mr-review taxonomy): `bug` blocks approval, `design` pushes
  back without blocking, `nit` is optional polish. The pipeline vetoes
  `approved` if any bug-severity comment exists (defense against inconsistent
  model output). Comment bodies: `🔴 **Bug**` / `🟡 **Design**` / `🟢 **Nit**`.
- **Self-PR handling**: GitHub rejects APPROVE and REQUEST_CHANGES on your own
  PR. The authenticated login is fetched at boot (`octocrab.current().user()`,
  no config field); matching PRs get a neutral COMMENT review whose body
  carries the outcome, and auto-merge still applies.
- **Auto-merge requires approval** (`approved && auto_merge`); a failed merge
  (branch protection etc.) is logged, not treated as a failed review.
- **Loop safeguards**: `max_attempts_per_sha` (default 3) caps failure retries,
  `max_reviews_per_pr` (default 20) caps lifetime reviews. Structurally our own
  actions can't re-trigger us (reviews/merges emit event types we ignore).
- **Auto-resolution**: open comments are replayed in the next round's prompt;
  the model returns indices of fixed ones; threads are resolved via the
  GraphQL `resolveReviewThread` mutation (REST cannot resolve threads — that's
  why posted comment database ids are tracked in state).
- **Secrets live in env** (`GITHUB_WEBHOOK_SECRET` required, `GITHUB_TOKEN`
  for API/private repos), never in config.toml (gitignored).
- Em-dashes and emojis are fine in GitHub-facing review text.

## Gotchas / insights

- The claude CLI validates `--json-schema` with Ajv, which does NOT support
  draft 2020-12 (schemars 1.x default). `review_schema()` generates
  **draft-07** via `SchemaSettings::draft07()`. Don't switch back to
  `claude_code::generate_schema` — it emits 2020-12 and the CLI exits 1.
- `Octocrab::builder().build()` and any octocrab call need a tokio reactor:
  the `#[launch]` fn must be `async`, and the builder is `!Send` so it must be
  scoped in a block (not held across an `.await`). Same reason webhook tests
  use `rocket::local::asynchronous` + `#[rocket::async_test]`.
- `octocrab.pulls().reviews().create_review()` takes the *response* model
  `ReviewComment` (unbuildable); reviews are posted via `crab.post` with raw
  JSON, which also enables `line`/`start_line` multi-line anchors.
- `repos` tests run real git against a local upstream (with
  `git update-ref refs/pull/7/head` to fake GitHub's PR refs) — cheap and real.
- State file is `<repos_path>/.state.json` by default; a PR whose sha failed
  repeatedly is retried on redelivery until the attempt cap; delete its entry
  to reset.

## Conventions

- Tests live in each module (`#[cfg(test)]`); every behavior change gets one.
- Structured logging via `tracing` (`RUST_LOG` to filter); no `println!`.
- Keep API shapes unless explicitly deciding to change them.

## TODO

- Implement labels (from the original spec).
