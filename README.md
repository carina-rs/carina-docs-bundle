# carina-docs-bundle

Validate and fully render Carina provider documentation archives. The registry and provider checkers share this crate's validation, rendering, resource limits, worker protocol, fingerprints and embedded navigation.

## Install and use

```sh
cargo install --locked --git https://github.com/carina-rs/carina-docs-bundle \
  --rev <full-commit> carina-docs-bundle
carina-docs-bundle check provider-docs.tar.gz \
  --namespace carina-rs --name aws --version 1.2.3
```

Pin the full commit of this repository used by the deployed registry; the registry's `Cargo.lock` records that Git revision. Supply the same namespace, provider name and intended version for equivalent verdicts: coordinates and embedded navigation affect rendered output, size and fingerprints. Without options, `check archive.tar.gz` uses `local/provider/0.0.0`.

Navigation metadata for registry-hosted providers lives in [`docs-navigation/`](docs-navigation/) because it affects rendered output. It is embedded through an explicit provider table. The installed executable runs outside the checkout and needs no registry configuration, AWS credentials or network access.

## Behavior

The checker validates and renders in a subprocess with fixed defaults: 384 MiB of address space, 20 CPU seconds and a 30-second wall timeout. Linux enforces address-space, CPU and core-file limits. macOS enforces CPU/core limits and the parent wall timeout without Linux address-space enforcement. Use Linux for resource-sensitive verdicts. Other platforms fail worker setup when Unix resource limits are unavailable.

Exit codes are `0` for successful validation and rendering, `1` for invalid or render-aborted bundles, and `2` for usage, I/O or infrastructure errors. The internal version-1 worker protocol retains setup-failure code `70`. Success requires rendering and parent-side response/navigation validation.

Archive limits are 16 MiB compressed, 4096 entries, 256 KiB per file, 64 MiB total file bytes, 80 MiB tar stream, 64 KiB per pax header, 128-byte slugs/titles, parse depth 64 and pipe-by-line product 250,000. Rendering permits 2 MiB per Markdown formatter output and 64 MiB total output. Resource and data-source pages must begin with an ATX H1 DSL title. Providers handle source frontmatter conversion before checking.

The library provides coordinates, subprocess rendering, immutable validated bundles and rendered pages, typed errors, highlighting, presentation paths, reference archives and fingerprint assembly. The `test-support` feature adds validated in-process rendering and fixture helpers. Production installation uses default features. Fetch, storage and publication integration belong to the registry.

## Local verification

From this repository's root, with `Cargo.lock` refreshed after dependency changes:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
python3 scripts/check-docs-bundle-dependencies.py
cargo test --locked --all-targets --all-features
cargo test --locked --doc --all-features
python3 -m unittest discover -s tests -p 'test_check_docs_navigation_coverage.py'
```

The dependency check inspects default/all-feature forward trees, including dev/build edges, rejects AWS/server packages and `tokio/full`, and verifies the legacy `rustls-webpki@0.101.7` is absent. CI also installs from the exact Git commit and runs the checker outside the checkout on a tiny reference archive, compares the heading slug fixture with the provider generator's copy, and checks navigation coverage against provider bundles.

To check a generated provider bundle against this repository's navigation metadata:

```sh
python3 scripts/check-docs-navigation-coverage.py carina-rs aws provider-docs.tar.gz
```
