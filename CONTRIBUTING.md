# Contributing

## Checks

`.github/workflows/ci.yml` runs, and a pull request passes before merging:

```sh
cargo deny check
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo clippy --all-targets --no-default-features --features ring --locked -- -D warnings
cargo test --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps --locked
cargo hack check --feature-powerset --at-least-one-of aws-lc-rs,ring --no-dev-deps
```

CI also builds at the MSRV (`cargo hack --rust-version check --all-features --locked`) and as docs.rs does (nightly). `cargo test --locked --test live -- --ignored` runs the read-only tests against production; CI runs them weekly and on demand. `advisories.yml` checks main's dependencies against RustSec every Monday.

## Vectors

`tests/vectors` holds what the tests check the crate against. LaMarge's repository owns them and writes them all at once; they are not edited here:

| File | Authority |
| --- | --- |
| `server-typed-data.json` | the server's typed-data builders |
| `eip712.json` | viem's digests of those typed data, as the TypeScript SDK signs them |
| `pins.json` | the TypeScript SDK's pinned deployments, protocol constants and Relay contracts, which `src/deployments.rs` and `src/protocol/relay.rs` equal |
| `relay.json` | Relay orders saved from live quotes, which the TypeScript SDK's checks also read |
| `lighter.json` | Lighter's signing vectors |
| `openapi.json` | the API's public contract |

From LaMarge's repository: `cd sdk && bun scripts/rust-vectors.ts <checkout of this repository>`, then a pull request here with the code the new vectors need. `--check` only compares.

## Releases

Versions are `0.MINOR.PATCH`: a feature, a breaking change or a newly pinned deployment raises `MINOR`, a fix only `PATCH`. The MSRV is raised only in a minor.

1. On a branch, set `Cargo.toml`'s version and date its `CHANGELOG.md` section (`## [X.Y.Z] - YYYY-MM-DD`); merge it.
2. Tag that commit of `main` `vX.Y.Z` and push the tag. `release.yml` checks the crate again, compares its API with the latest published version (`cargo semver-checks`), publishes it to crates.io by trusted publishing and creates the GitHub release from the changelog.

Trusted publishing needs the crate to exist: the first version is published once by hand with a crates.io token (`cargo publish --locked` from the merged commit), whose tag then only creates the GitHub release. The crate's trusted publisher is then configured on crates.io: repository `snowbl-capital/lamarge-sdk-rs`, workflow `release.yml`, environment `crates-io`.

## Commits

`type: what the change does`, in the present tense. Types: `feat`, `fix`, `refactor`, `test`, `docs`, `style`, `chore`. The body says why, when the diff does not.
