# Contributing

## Build

Use the stable Rust toolchain; Rust 1.98 is the toolchain tested for this release. Build the
workspace with:

```sh
cargo build --workspace --locked
```

`./scripts/install.sh` does the whole setup (prerequisite checks, CLI build, images, `init`);
see the README. To build only the runtime images, enable Docker BuildKit and run:

```sh
./scripts/build-images.sh
```

The script builds the capsule base, five tool images, the merged all-tools image, and the
gateway image with its Rust broker build. The merged image is about 2.4 GB. The first build
can take a while.

To move a tool to a new version, run `./scripts/bump-tool.sh <tool> <version>`. It records the
version, the npm tarball hash, and a `resolved_before` time that fixes the tool's dependency
versions, then updates the Dockerfile defaults. Rebuild the images and run the tool in a real
session before submitting the change.

## Checks

Before submitting changes, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Docker-backed tests are skipped unless `CAPSCODE_TEST_DOCKER=1` is set. Enable them only when
a Docker runtime is available:

```sh
CAPSCODE_TEST_DOCKER=1 cargo test --workspace --locked
```

## Style

Follow the surrounding Rust style and keep user-facing errors actionable. The CLI denies
`clippy::print_stdout` and `clippy::print_stderr` outside its output module: send data to stdout
and diagnostics to stderr through the existing UI emitters. Preserve the credential boundary
and fail-closed network behavior when changing runtime or broker code.

## Documentation

The pages under `docs/` are published to GitHub Pages by `.github/workflows/docs.yml`. To
preview the site locally:

```sh
python3 -m venv .venv-docs
.venv-docs/bin/pip install --require-hashes -r docs/requirements.txt
.venv-docs/bin/mkdocs serve
```

Links between pages use relative `.md` paths so they work both on GitHub and on the site.

## Conduct

Participation follows the [code of conduct](CODE_OF_CONDUCT.md).
