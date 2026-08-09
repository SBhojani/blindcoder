# Contributing

Thanks for your interest. blindcoder is early and opinionated; the fastest path to a merged
change is to align on intent first.

## Before writing code

**Open an issue first** for anything beyond a small fix, and say what you want to change and
why. blindcoder has a few core invariants (see [AGENTS.md](AGENTS.md)); a quick issue
saves you from building something that conflicts with them.

## Development

```sh
cargo build --workspace
cargo test  --workspace
cargo fmt --all
cargo clippy --workspace
```

Pass `--workspace` every time. The repo root is a package (the `blindcoder` binary) with the crates
under `crates/*` as path dependencies, not a virtual workspace, so a bare `cargo test` runs only the
binary's tests and skips every sub-crate (`selector`, `store`, `backend`, …) where most of the logic
lives. If you see a single `test result:` line (for `unittests src/main.rs`) instead of one per
crate, you left it off.

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
