# CLI release assets

This document names the files a CapsCode CLI release (`v<version>`) publishes and
says how `scripts/install.sh` and `capscode update --cli` find and check them.
The release workflow (`.github/workflows/release-cli.yml`, logic in
`scripts/release/`) and both clients follow it. The channel manifest that points
clients at a release is described in [channel-format.md](channel-format.md).

## Assets

Every asset is attached to the GitHub release for tag `v<version>` and downloaded
from `https://github.com/<channel>/releases/download/v<version>/<asset>`. For
`capscode update --cli`, `<channel>` is `update.channel` (default
`sayre4ux/capscode`); the installer always uses `sayre4ux/capscode`. `<version>`
is the workspace version in `Cargo.toml`, without the `v`.

| Asset | Contents |
|---|---|
| `capscode-<version>-macos-universal` | The CLI for macOS, arm64 and x86_64 in one file (`lipo`), ad-hoc signed with identifier `dev.capscode.cli`. No Apple Developer ID, no notarization. |
| `capscode-<version>-linux-x86_64` | The CLI for Linux on x86-64, linked against glibc 2.35 or older (runs on Ubuntu 22.04+ and Debian 12+). |
| `capscode-<version>-linux-aarch64` | The same for Linux on arm64. |
| `capscode-<version>-build-context.tar.gz` | The files `scripts/build-images.sh` needs to build every image, for installs without a checkout (see below). |
| `capscode-<version>-release-notes.md` | The `CHANGELOG.md` section for the release; `capscode update --cli` shows it before asking. |
| `capscode-<version>-src.tar.gz` | `git archive` of the tag. |
| `SHA256SUMS` | SHA-256 of every asset above. |
| `capscode-<version>-attestation.sigstore.json` | The GitHub artifact attestation bundle covering every asset above, `SHA256SUMS` included. |

Binaries are bare executables, not archives. There is no Windows build.

### `SHA256SUMS`

`sha256sum` output: one line per asset, `<64 lowercase hex>`, two spaces, the
asset name, sorted by name. Clients accept ` *` instead of the two spaces, and
refuse a file with a malformed line, a name listed twice, or no line for an asset
they download. Every downloaded file must match its line before anything is
written to its final place.

### Attestation

`actions/attest-build-provenance` signs one provenance statement whose subjects
are every asset above. Verify a download with:

```sh
gh attestation verify <asset> --repo sayre4ux/capscode \
  --signer-workflow sayre4ux/capscode/.github/workflows/release-cli.yml
```

`--signer-workflow` pins the release workflow, so an attestation produced by any
other workflow of the repository does not vouch for a release asset.

## The image build context

Images are never published; every user builds them. The build context tarball
carries exactly what `scripts/build-images.sh` reads: `Cargo.toml`, `Cargo.lock`,
`rust-toolchain.toml`, `LICENSE`, `crates/`, `images/`, `providers/`,
`scripts/build-images.sh`, and `scripts/smoke-tui.sh`. The gateway image compiles
the broker inside Docker, so a binary install needs no Rust on the host. The
tarball never contains `.git`, `target/`, or any file git ignores.

Format: gzip-compressed ustar. Every entry is a regular file under
`capscode-<version>-build-context/`; there are no directory, link, or device
entries. `scripts/release/build-context.sh` produces it and
`scripts/release/test-build-context.sh` checks that it holds every Dockerfile
`COPY` source.

Clients unpack it to `<CapsCode home>/build/<version>/` (the CapsCode home is
`$CAPSCODE_HOME`, or `~/.capscode`). They refuse the whole archive if any entry is
outside the top-level directory, has an absolute path or a `..` component, or is
a link or a device file, has more than 20,000 entries, or unpacks to more than
64 MiB, and they unpack into a temporary directory that replaces the final one
only when complete.

The CLI looks for its image build script in this order:

1. `CAPSCODE_BUILD_IMAGES`, the path of a `build-images.sh` to run;
2. `<CapsCode home>/build/<own version>/scripts/build-images.sh`;
3. the source checkout the binary was built from, if it still exists and belongs
   to the current user;
4. otherwise it stops and says how to restore the files (rerun the installer).

`capscode toolchain build` runs the script found this way.

## Who downloads what

`scripts/install.sh` (binary install) reads `channel.json` with a small JSON
parser written in `awk` (no `jq` or Python needed). It requires valid JSON with
`schema` equal to `1`, `cli.tag` equal to `v` plus `cli.version`, each field it
reads exactly once, and no escaped keys. It takes the newest version from
`cli.version` unless `--version` is given, and refuses a version listed in
`cli.yanked_versions` either way. It then downloads `SHA256SUMS`, the binary for
the host, and the build context. With
`--verify-attestation` it also requires `gh attestation verify` to accept the
binary and the build context. On macOS it requires `codesign --verify --strict`
to pass. It runs the binary's `--version` before installing it.

`capscode update --cli` downloads the same three files plus the release notes,
using the same HTTPS client and host allowlist as `capscode update`, with a
64 MiB cap for the binary and 16 MiB for the build context. With
`update.verify_with_gh = true` it runs the `gh attestation verify` command above
on the binary and the build context.

Both refuse a version that is not a plain `X.Y.Z` (no prerelease or build
suffix). A prerelease tag is published as a GitHub prerelease and never written
to `channel.json`.

### Test mirror (`CAPSCODE_RELEASE_BASE_URL`)

For the installer's own tests only, `CAPSCODE_RELEASE_BASE_URL` replaces
`https://github.com/<channel>/releases/download` with a mirror laid out the same
way (`<base>/v<version>/<asset>`, plus `<base>/channel.json`). It accepts only
`https://` and `file://` URLs and changes where files come from, never how they
are checked: the `SHA256SUMS` match, the archive checks, the signature check, and
`--verify-attestation` all still apply. `scripts/test-install.sh` uses it.
