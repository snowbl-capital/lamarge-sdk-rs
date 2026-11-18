# Contributing

This repository mirrors each release of the `lamarge` crate from LaMarge's repository, where the crate is developed next to the server and the API contract. Issues are welcome here. Pull requests are not merged here: a maintainer makes the change upstream, and the next release brings it back.

## Checks

`.github/workflows/ci.yml` runs them on every push:

```sh
cargo deny check
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo clippy --all-targets --no-default-features --features ring --locked -- -D warnings
cargo test --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps --locked
cargo hack check --feature-powerset --at-least-one-of aws-lc-rs,ring --no-dev-deps
```

CI also builds at the MSRV (`cargo hack --rust-version check --all-features --locked`) and as docs.rs does (nightly). `cargo test --locked --test live -- --ignored` runs the read-only tests against production; CI runs them weekly and on demand.

## Vectors

`tests/vectors` holds copies of what LaMarge's repository is the authority for; they change only there:

| File | Authority |
| --- | --- |
| `openapi.json` | the API's public contract |
| `lighter.json` | Lighter's signing vectors, which the server's port also passes |
| `server-typed-data.json` | the server's typed-data builders |
| `eip712.json` | viem's digests of those typed data, as the TypeScript SDK signs them |
| `pins.json` | the TypeScript SDK's pinned deployments, protocol constants and Relay contracts, which `src/deployments.rs` and `src/protocol/relay.rs` equal |
| `relay.json` | Relay orders saved from live quotes, which the TypeScript SDK's checks also read |

## Releases

Versions are `0.MINOR.PATCH`: a feature, a breaking change or a newly pinned deployment raises `MINOR`, a fix only `PATCH`. The MSRV is raised only in a minor.

A release lands upstream: `Cargo.toml`'s version and its dated `CHANGELOG.md` section (`## [X.Y.Z] - YYYY-MM-DD`) merged into LaMarge's `main` commit the crate here and tag it `vX.Y.Z`. The tag runs `release.yml`: it checks the crate again, compares its API with the latest published version (`cargo semver-checks`), publishes it to crates.io by trusted publishing and creates the GitHub release from the changelog.

Trusted publishing needs the crate to exist, so versions are published with a crates.io token stored as the `crates-io` environment's `CARGO_REGISTRY_TOKEN` secret while one is set. Once the crate's trusted publisher is configured on crates.io (repository `snowbl-capital/lamarge-sdk-rs`, workflow `release.yml`, environment `crates-io`), the secret is deleted. A version already on crates.io only gets its GitHub release.
