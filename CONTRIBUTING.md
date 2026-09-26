# Contributing

Issues and pull requests are welcome. For anything bigger than a fix, open an issue first so we
can agree on the shape before you write it.

## Setup

Rust (stable) and [bun](https://bun.sh).

```
cd ui && bun install && bun run build && cd ..
cargo run -p op-cli -- serve                        # http://127.0.0.1:7878
cargo run -p collar-sim -- --count 12               # after creating a herd in the app
cd ui && bun run dev                                # UI with hot reload on :5173
```

## Before you open a pull request

```
cargo fmt --all
cargo test --workspace
cd ui && bun run build
```

CI runs the same on Linux and macOS.

- Keep changes small and focused, with a test where the behaviour can be tested.
- No mocks, stubs or placeholder data in shipped code. `collar-sim` speaks the real protocol;
  use it instead of faking collars.
- If you change an endpoint or a type, update `docs/API.md` in the same pull request.
- Write docs and UI text plainly, for farmers and builders.

## Licence

By contributing you agree that your work is licensed like the code it touches: AGPL-3.0 for the
app, Apache-2.0 for `crates/op-protocol` and `crates/op-geo`.
