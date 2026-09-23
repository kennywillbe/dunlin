# Contributing to dunlin

dunlin sends alerts, so a missed alert or a false one is the worst bug it can
have. A change that makes either more likely will not be merged.

## Getting set up

```sh
git clone https://github.com/kennywillbe/dunlin
cd dunlin
cargo build
cp dunlin.example.toml dunlin.toml
cargo run -- --config dunlin.toml
```

Rust stable is enough. The MSRV is `rust-version` in `Cargo.toml` and CI
checks it.

The test suite needs no network, Docker or systemd: probes run against local
mock servers, the Docker and systemd clients sit behind traits, and `/proc` is
read from fixtures in `tests/fixtures`.

## Before every push

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

That is what the `fmt + clippy + tests` job runs on Linux and macOS. CI also
builds the container image and runs `cargo check` on the MSRV toolchain; if you
touched the `Dockerfile`, run `docker build .` too.

Every bug fix needs a test that fails without the fix.

## Commits and pull requests

Pull requests are squash-merged, so the pull request title becomes the commit
subject on `main`, and release-please turns it into the changelog line and the
next version. The title has to be a
[Conventional Commit](https://www.conventionalcommits.org/): `feat:`, `fix:`,
`docs:`, `chore:`, and `feat!:` for anything that breaks an existing config or
database. A check fails the pull request if it does not parse.

Fill in [the pull request template](.github/pull_request_template.md). The web
UI does that for you; `gh pr create` does not.

## Style

- Comments explain why, not what. No commented-out code.
- No `unwrap()` or `expect()` outside tests unless a comment right there says
  why it cannot fail.
- Build only what the change needs.
- Secrets (password hash, bot tokens, webhook headers, heartbeat tokens) never
  reach logs, errors or test fixtures.

## Reporting bugs

Use the bug report template. Include `dunlin --version`, how you run it, your
config with secrets removed and the log lines around the problem
(`RUST_LOG=debug`).
