# AGENTS.md — rules for AI agents working on this repository

This file defines hard rules for any AI agent developing inside
`lobsterai-proxy`. Read it fully before making changes.

## Secrets — absolute rules

- **Never commit secrets.** `auth/`, `auths/`, `lobsterai-*.json`,
  `config.toml`, `.env`, access tokens, refresh tokens, real JWTs, raw UIDs
  must never appear in commits, logs, test fixtures, or docs.
- `auth/` and `config.toml` are **local only**. They are git-ignored; do not
  force-add them (`git add -f` is forbidden for these paths).
- Never print credential values into the terminal, CI logs, or files.
- Real LobsterAI credentials must never be used in tests or CI. Tests run
  against the local mock backend only.

## Project targets

- The ONLY upstream model is **`deepseek-flash`**, proxied through the
  NetEase Youdao **LobsterAI** backend (`https://lobsterai-server.youdao.com`).
- Target clients: **Claude Code** (Anthropic Messages API) and **Grok Build**
  (OpenAI Responses API).

## Behavioral invariants (code + tests must preserve)

- Never replay/regenerate a generation after any semantic output was
  streamed to the client.
- HTTP 401 → single-flight refresh of the SAME account, retry exactly once;
  a second 401 stops.
- HTTP 429 / out-of-credits → cooldown, then failover to another account.
- 403 / 5xx / transport errors → stop immediately; no replay, no failover.
- Historical reasoning may be stripped; text, tool calls, tool ids, and tool
  results are never modified.
- Tool schemas pass through byte-identical. No truncation of tool results or
  user content.
- Every log line and error path passes through `redaction`.

## Git workflow

**NEVER commit directly to `main`.** Direct pushes are blocked by branch
protection. For every change:

```sh
git switch main
git pull --ff-only
git switch -c <type>/<short-name>     # feat/ fix/ docs/ chore/ ci/ ...

# ... make changes ...

cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all

git add <files>
git commit -m "<type>: summary"
git push -u origin <type>/<short-name>
gh pr create
gh pr checks          # wait for all required checks to pass
gh pr merge --squash --delete-branch
```

- PR titles and commit messages use Conventional Commits
  (`feat:`, `fix:`, `perf:`, `refactor:`, `test:`, `docs:`, `chore:`, `ci:`).
- Never use `--admin` to bypass protection, never force-push `main`, never
  temporarily disable protection to push.

## Repository hygiene

- Do not create scratch/analysis files in the repository root. Analysis stays
  in the agent's context; long-lived documentation goes in `docs/`.
- Do not run `cargo update` without a concrete reason.
- CI is credential-free; do not add repository secrets or variables.
