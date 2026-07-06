# Contributing

Thanks for your interest. blindcoder is early and opinionated; the fastest path to a merged
change is to align on intent first.

## Before writing code

**Open an issue first** for anything beyond a small fix, and say what you want to change and
why. blindcoder has a few load-bearing invariants (see [AGENTS.md](AGENTS.md)); a quick issue
saves you from building something that conflicts with them.

## Development

```sh
cargo build --workspace
cargo test  --workspace
cargo fmt --all
cargo clippy --workspace
```

Or drop into the reproducible dev shell with `nix develop`. Nix is only for development and
building — it is never required to run blindcoder.

Guidelines:

- Keep the **selector** pure (no I/O, no clock, no ambient RNG) and property-tested.
- Preserve the **append-only** store semantics — corrections supersede, never edit.
- Keep the tree **vendor-neutral**: no AI-assistant names in commits or committed files, and
  never commit user state (the DB, wire archives, real config, or secrets).
- Run `cargo fmt` and keep `cargo clippy` clean.

## License

By contributing you agree that your contributions are licensed under the project's
[Apache-2.0](LICENSE) license. There is no separate CLA.
