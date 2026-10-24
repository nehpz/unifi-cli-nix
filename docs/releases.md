# Releases

Vership is the release control plane for `unifi-cli`. GitHub Actions is the
execution plane for cross-platform builds, publication, provenance, and
downstream packaging.

## Create a release

Start from a clean, up-to-date `main` branch and run:

```sh
vership preflight
vership bump patch --prepare # or minor/major
```

Vership runs `make check`, updates the package version and changelog, and creates
the Conventional Commit release commit locally. Review the prepared release and
obtain approval to push and publish, then finish that same version without
bumping again:

```sh
vership release
```

The tag starts the release workflow. Wait for all publication and downstream
packaging jobs to finish, then run `vership verify` and confirm every intended
target, including the Homebrew tap.

Cargo and Python package versions are kept synchronized as one release unit.

## Homebrew

The release workflow maintains `rvben/homebrew-tap` using the
`HOMEBREW_TAP_DEPLOY_KEY` secret. The formula template is
`packaging/unifi-cli.rb.in`; `scripts/homebrew_formula.py` fills in the version
and checksums from the release artifacts. It verifies every archive's checksum
and requires both executables before generating the formula. `unifi-cli` launches
the sibling `unifi` executable, so both must be shipped together on every platform.

Before publication, the workflow installs the native macOS archive through
Homebrew and tests both commands and generated Bash, Zsh, and Fish completions.
This check also runs during release dry runs. Pull requests test the same formula
with locally built native binaries. Run it locally with Python 3.11 or newer and
Homebrew, on a machine without an existing Homebrew installation of `unifi-cli`:

```sh
make packaging-test
cargo build --locked --bins
python3 scripts/test_homebrew.py --bin-dir target/debug
```

The smoke test installs without linking commands into the global prefix and
removes its test installation afterward. Homebrew formula generation and tap
updates happen automatically after the GitHub release is published. If only the
tap update fails, retry that job; do not create another release. If the archives
themselves are incomplete, publish a patch release rather than replacing assets.

## Failure policy

If a release fails before any release, artifact, package, checksum, or
attestation becomes public, delete and recreate the brief tag and retry the
same version. Once anything was published, preserve the tag and issue a patch
release for release-content corrections. Registry and downstream-packaging
jobs are separate recovery domains; retry only the failed job when possible.

## Dry runs

The Release workflow can be dispatched from `main` with `dry_run` enabled. It
uses the package version unless an explicit `version` is supplied, builds the
real artifacts, and exercises registry validation without publishing packages,
creating a GitHub release, generating attestations, or updating downstream
packaging.
