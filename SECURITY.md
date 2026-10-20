# Security policy

## Reporting a vulnerability

Report suspected vulnerabilities privately through GitHub's private vulnerability reporting:
open the repository's **Security** tab and select **Report a vulnerability**. Please include
the affected version, a clear description, and steps to reproduce where possible. Do not open a
public issue for an unpatched vulnerability.

## Supported versions

The supported release line is **0.1.x**.

## Verifying a release

Every CLI release publishes `SHA256SUMS` next to its binaries, image build context, source
tarball, and release notes; [docs/release-assets.md](docs/release-assets.md) lists them. The
installer and `capscode update --cli` refuse any download that does not match it, and any CLI
release that the release channel lists as withdrawn (`cli.yanked_versions` in `channel.json`). To
check a download by hand:

```sh
sha256sum --check --ignore-missing SHA256SUMS      # Linux
shasum -a 256 --check --ignore-missing SHA256SUMS  # macOS
```

Each release asset, `SHA256SUMS` included, and each toolchain lock also carries a GitHub
artifact attestation. Verify it against this repository and the workflow that must have produced
it:

```sh
gh attestation verify capscode-0.1.1-macos-universal --repo sayre4ux/capscode \
  --signer-workflow sayre4ux/capscode/.github/workflows/release-cli.yml
gh attestation verify capscode-0.1.1-build-context.tar.gz --repo sayre4ux/capscode \
  --signer-workflow sayre4ux/capscode/.github/workflows/release-cli.yml
gh attestation verify capscode-0.1.1-src.tar.gz --repo sayre4ux/capscode \
  --signer-workflow sayre4ux/capscode/.github/workflows/release-cli.yml
gh attestation verify toolchain.lock.toml --repo sayre4ux/capscode \
  --signer-workflow sayre4ux/capscode/.github/workflows/release-toolchain.yml
```

`install.sh --verify-attestation` and `update.verify_with_gh = true` run the same check and fail
closed when `gh` is missing or rejects the file.

The macOS binary is ad-hoc signed and not notarized: CapsCode has no Apple Developer ID. Its
signature proves only that the file is intact (`codesign --verify --strict` passes), not who
built it; the checksum and the attestation are what tie it to this repository's release
workflow. Download it with `install.sh` (curl adds no quarantine flag); a browser download is
blocked by Gatekeeper.

The attestation identifies the repository workflow that produced an artifact;
it does not prove the source or workflow was benign. Someone who compromises
this repository can publish a malicious CLI binary or source with a valid
checksum and attestation, or publish a lock selecting a bad version of an
already allowlisted package or image. The CLI's compiled-in package and image allowlists prevent a lock from
introducing a new package or image repository, and the user's build still checks
the package integrity and image digest. Those checks do not protect against a
compromised allowlisted upstream or compromised repository workflow.

## Scope

CapsCode's security boundaries, assumptions, and known residual risks are described in the
[threat model](docs/threat-model.md). Reports about the capsule/host boundary, credential
handling, the broker, or network containment are in scope.
