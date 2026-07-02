# issuegator

Rust TUI GitHub issue explorer for the current repository.

## Install

```sh
brew install yarden-zamir/tap/issuegator
```

## Run

From a GitHub-backed Git repository:

```sh
issuegator
```

`issuegator` uses `gh` and the current repo's `origin` remote.

## Build

```sh
cargo build --release
```

## Check

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## License

MIT
