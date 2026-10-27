# Contributing

Issues and pull requests are welcome. For anything bigger than a fix, open an issue first so we
can agree on the shape before you write it.

## Setup

Rust (rustup installs the version in `rust-toolchain.toml`) and [bun](https://bun.sh) (the
version in `.bun-version`).

```
cd ui && bun install && bun run build && cd ..
cargo run -p op-cli -- serve                        # http://127.0.0.1:7878
cargo run -p collar-sim -- farm                     # a simulated farm of its own on :7991
cd ui && bun run dev                                # UI with hot reload on :5173
```

## Before you open a pull request

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd ui && bun test && bun run build
```

CI runs the same on Linux and macOS, builds the server on Windows, and also checks licences
and advisories (`cargo deny check`, `bun audit`), that the version files agree
(`scripts/set-version.py --check`), and that op-protocol's test vectors match the firmware's in
opencollar.

- The clippy lints allowed in `Cargo.toml` (`[workspace.lints.clippy]`) are a backlog, not a
  policy: fix one and delete its line.
- A change people will notice gets a line under Unreleased in `CHANGELOG.md`.
- Security problems go through [`SECURITY.md`](SECURITY.md), not issues.

- Keep changes small and focused, with a test where the behaviour can be tested.
- No mocks, stubs or placeholder data in shipped code. `simulation/collar-sim` speaks the real
  protocol; use it instead of faking collars, on a farm of its own (`simulation/README.md`).
- If you change an endpoint or a type, update `docs/API.md` in the same pull request.
- Write docs and UI text plainly, for farmers and builders.

## Licence

By contributing you agree that your work is licensed like the code it touches: AGPL-3.0 for the
app, Apache-2.0 for `crates/op-protocol` and `crates/op-geo`.
