# 🛡️ Repo Guardian

Automated pull-request reviews for your GitHub repos, powered by the
[Claude Code CLI](https://docs.anthropic.com/en/docs/claude-code). Open or
push to a PR and Guardian clones the repo, reads the actual changes, and posts
a single review with severity-tagged inline comments — approving and even
merging when everything checks out.

## How it works

```
GitHub PR (opened / synchronize)
        │ webhook (HMAC-validated)
        ▼
  repo-guardian (Rocket server)
        │ 1. claim (PR, head sha)        — idempotent, capped
        │ 2. clone/fetch + PR worktree   — reviewer sees the PR's code
        │ 3. claude --print + schema     — structured verdict
        ▼
  One PR review
        ├── 🔴 Bug / 🟡 Design / 🟢 Nit inline comments
        ├── ✅ Approve / ⚠️ Request changes
        ├── ♻️ resolves earlier comments the new push fixed
        └── 🚀 auto-merge (optional)
```

Each PR's state is persisted (JSON), so restarts don't re-review, webhook
redeliveries are deduplicated, and open comments are carried into the next
round — when a new push fixes one, its thread is resolved automatically.

If the PR author is the same account Guardian runs as (GitHub forbids
approving your own PR), the verdict is posted as a neutral comment review
instead — and auto-merge still works.

## Setup

Requirements: `git`, the `claude` CLI (authenticated), and a GitHub token.

1. **Configure** — copy `config.toml.example` to `config.toml`:

   ```toml
   auto_merge = false
   repos_path = "/Users/you/.local/share/repo-guardian/repos"

   # optional (defaults shown)
   # state_path = "<repos_path>/.state.json"
   # max_attempts_per_sha = 3   # failed-review retries per head sha
   # max_reviews_per_pr = 20    # lifetime review cap per PR (loop safeguard)
   ```

2. **Environment**:

   | Variable | Required | Purpose |
   | --- | --- | --- |
   | `GITHUB_WEBHOOK_SECRET` | yes | shared secret for webhook validation |
   | `GITHUB_TOKEN` | recommended | API calls, private clones, self-PR detection |
   | `RUST_LOG` | no | log filter, e.g. `repo_guardian=debug` |

3. **Run**:

   ```sh
   cargo run --release
   ```

4. **Webhook** — in the repo settings add a webhook pointing to
   `https://<host>/webhook/gh`, content type `application/json`, the same
   secret, and the *Pull requests* event. Deliveries without a valid
   `X-Hub-Signature-256` are rejected with 401.

## Review anatomy

Every round posts exactly one review:

- **Body** — verdict, a severity/count table, and how many earlier comments
  the push resolved.
- **Inline comments** — anchored to the changed lines, each leading with its
  severity badge:
  - 🔴 **Bug** — wrong behavior, regressions, broken invariants. Blocks
    approval (enforced server-side, even if the model says "approve").
  - 🟡 **Design** — scope creep, API shape, boundary leaks. Doesn't block.
  - 🟢 **Nit** — minor polish.

## Development

```sh
cargo test          # unit + integration tests (uses real git locally)
cargo clippy --all-targets
```

The reviewer is generic over the CLI runner, so tests inject a fake `claude`
and assert on the prompt and parsed output — no network, no tokens needed.

## License

MIT
