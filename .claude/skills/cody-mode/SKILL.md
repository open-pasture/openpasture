---
name: cody-mode
description: >-
  Cody Menefee's agent style, layered on poteto-mode. Use for Cody, /cody-mode,
  or requests to work in Cody's style.
disable-model-invocation: true
---

# Cody mode

Load the **poteto-mode** skill and follow it. This file overrides poteto-mode and every playbook it routes to wherever they conflict.

## Approval gates

Do none of these without the user's explicit approval of that specific action. Approval of one action never covers the next.

- Push to `main` or any protected branch.
- Merge a PR.
- Publish an OTA or EAS update.
- Deploy to staging or production.
- Run live E2E tests against staging or production.
- Use admin rights or any branch-protection bypass.
- Push a fix wave during a babysit.

## Overrides of pstack defaults

- Open every PR as a draft: `gh pr create --draft`. This replaces `draft: false` in the Opening a PR playbook.
- Run the Babysit playbook only in `check` or `threads-only` mode. Never `drive`.
- Skip the `/deslop` step. The plugin is not installed.
- Skip the **no-comments** step. These repos keep contracts and rationale in comments on purpose. Never strip comments.

## Branches

- Start every task on a fresh branch cut from the current origin default branch. Run `git fetch origin` first.
- Never touch other open stacked draft PRs or their branches.

## Toolchain

- Follow each repo's AGENTS.md and lockfile for the package manager.
- In the Bun repos, relay and the openpasture UI, use `bun` and `bunx`. Never `npm` or `npx`.
- openpasture-mobile is an Expo app. Add dependencies with `bunx expo install`.
- Rust: run `cargo fmt`, `cargo clippy`, and `cargo test`.

## Verified facts only

- Back every "done", "passes", or "works" claim with the command you ran and its output, a screenshot path, or a stored value.
- A green build does not prove behavior. Exercise the behavior itself.
- Label any claim without that evidence "unverified".

## Spatial, CAD, and visual requests

Before modeling, restate the requested orientation and dimensions in your own words per the **in-your-own-words** skill.

## Execution

- Do the whole task end to end yourself. Don't hand work back to the user.
- Optimize for speed. Spawn the fewest subagents that still produce verified work.
- Review panels have two reviewers. Their models are on the `interrogate reviewers` line of `~/.claude/rules/pstack-models.md`.

## Replies

- Keep replies short. Lead with the outcome.
- Use plain words. Put one item on each line.
- Report times in US Central with a "CT" label. Convert UTC and `Z` timestamps.
- End with the decisions the user must make, each with your recommendation.

## This repo

- Package managers: cargo for the Rust workspace (toolchain from `rust-toolchain.toml`), bun for `ui/` and `scripts/shotdiff/` (version from `.bun-version`). Never npm or npx.
- The gates CI runs (`.github/workflows/ci.yml`), in the order to run them locally:
  - `cargo fmt --all --check`
  - `cd ui && bun install --frozen-lockfile && bun run typecheck && bun run lint && bun run test && bun run build` (op-server embeds `ui/dist`, so build it before cargo)
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`
  - `cargo test --workspace --locked`
  - `python3 scripts/set-version.py --check`
  - `cargo deny --locked check` and `python3 scripts/deny-ignores.py`
  - `scripts/compare-vectors.sh <opencollar checkout at OPENCOLLAR_REF>`
- Local stack only: `cargo run -p op-cli -- serve` (127.0.0.1:7878, a real farm's port), `target/debug/collar-sim farm` (its own server on :7991), `cd ui && bun run dev` (:5173). The garage stack is `~/bin/opdev`. Never point a script, simulator or test at a staging or production host.
