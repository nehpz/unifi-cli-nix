# Nix packaging

`nehpz/unifi-cli-nix` is a fork of [`rvben/unifi-cli`](https://github.com/rvben/unifi-cli)
that adds Nix flake packaging and publishes to FlakeHub as `rzp/unifi-cli-nix`.
All CLI features and docs come from upstream; see [README.md](README.md).

## Try it

```bash
nix run "https://flakehub.com/f/rzp/unifi-cli-nix/*.tar.gz" -- --version
nix run github:nehpz/unifi-cli-nix -- --version
```

Install into your profile (`fh add` edits a `flake.nix`, it does not install):

```bash
nix profile add "https://flakehub.com/f/rzp/unifi-cli-nix/*.tar.gz"
```

## Add as a flake input

FlakeHub is the primary form. `*` means latest:

```nix
inputs.unifi-cli.url = "https://flakehub.com/f/rzp/unifi-cli-nix/*.tar.gz";
```

Pin to an exact rolling release with `=0.1.N+rev-<sha>`:

```nix
inputs.unifi-cli.url = "https://flakehub.com/f/rzp/unifi-cli-nix/=0.1.N+rev-<sha>.tar.gz";
```

GitHub, as an alternative:

```nix
inputs.unifi-cli.url = "github:nehpz/unifi-cli-nix";
```

## Use it

Apply the overlay (recommended: no `${system}` indexing):

```nix
nixpkgs.overlays = [ inputs.unifi-cli.overlays.default ];
# NixOS
environment.systemPackages = [ pkgs.unifi-cli ];
# home-manager
home.packages = [ pkgs.unifi-cli ];
```

Or take the package directly:

```nix
# NixOS
environment.systemPackages = [ inputs.unifi-cli.packages.${pkgs.stdenv.hostPlatform.system}.default ];
# home-manager
home.packages = [ inputs.unifi-cli.packages.${pkgs.stdenv.hostPlatform.system}.default ];
```

`inputs` must be passed into module scope via `specialArgs` (NixOS) or `extraSpecialArgs` (home-manager); neither is automatic.

## Binaries

The package installs both `unifi` (primary) and `unifi-cli`. `nix run` gives
you `unifi`.

## Outputs

Supported systems: `x86_64-linux` and `aarch64-darwin`. CI runs `ubuntu-latest` and `macos-latest`, mapping one-to-one onto those two.

`x86_64-darwin` is dropped by Nixpkgs 26.11; evaluating it throws. `aarch64-linux` is intentionally not enumerated — it builds, but no consumer targets it and CI does not verify it. `overlays.default` is system-agnostic and `meta.platforms` is `unix`, so a consumer on any unix system — including `aarch64-linux` — can still apply the overlay and get a working `pkgs.unifi-cli`. Only the `packages.<system>.*` attributes are limited to the two enumerated systems.

- `packages.<system>.{default,unifi-cli}`
- `overlays.default`
- `devShells.<system>.default`
- `checks.<system>.unifi-cli`
- `formatter.<system>`

## Local development

```bash
nix develop
nix build .#default
nix flake check
```

`.envrc` (`use flake`) makes direnv pick the shell up automatically.

`nix develop` provides nixpkgs' Rust toolchain (currently 1.97.1), while `.mise.toml` and the retained `ci.yml` pin 1.96.0 and `make lint` runs `cargo clippy -- -D warnings`. Because clippy's default lint set and rustfmt's output are versioned with the toolchain, formatting and lint results from `nix develop` can differ from CI. `mise` is authoritative for `cargo fmt`/`clippy`; the Nix shell is for building and for working on the packaging.

## Versioning

FlakeHub gets *rolling* releases (`0.1.<commit-count>+rev-<sha>`) on every
push to `main`. These do not track the upstream crate version — the crate
version lives in `Cargo.toml` and is read dynamically by the flake.

Upstream's `vX.Y.Z` tags point at commits that contain no `flake.nix`, so they
cannot be republished as-is. The README "Releasing" runbook, `docs/releases.md`,
and the `make release-*` targets are upstream-only and inert in this fork; the
only published artifact is the FlakeHub flake.

## Automation

- `.github/workflows/nix.yml` — `nix flake check` + `nix build` + a `--version` smoke run on `ubuntu-latest` and `macos-latest` (the two supported systems) for every PR and push to `main`; on `main` it then calls the publish workflow.
- `.github/workflows/flakehub.yml` — reusable FlakeHub publish (`visibility: public`, `rolling: true`, `include-output-paths: true`), authenticated by GitHub OIDC, so there are **no secrets to configure**. The published name is set explicitly as `rzp/unifi-cli-nix`.
- `.github/workflows/auto-update.yml` — daily: merge `rvben/unifi-cli` `main`; weekly (Mon): also `nix flake update`. The candidate commit is force-pushed to an `auto-update` staging branch, verified with `nix flake check` + `nix build` + a `--version` smoke run on **both** `ubuntu-latest` and `macos-latest`, and only the verified SHA is promoted to `main` and published. A Darwin-only breakage therefore leaves `main` untouched and the staging branch in place for inspection.
- If `promote` succeeds but the FlakeHub publish fails, the run goes red and the next scheduled run will see no new commits and will NOT retry the publish; recover by manually dispatching `.github/workflows/flakehub.yml` (it accepts a `sha` input for exactly this). This is deliberate — a durable retry state machine was judged not worth the complexity for a visible, one-click-recoverable failure.
- Upstream syncs never need a human, and never need a token with `workflow` scope (`GITHUB_TOKEN` cannot have one — there is no such `permissions:` key, so pushing any upstream change under `.github/workflows/` is rejected outright). `auto-update.yml` resolves by ownership instead: `.github/` is restored from the fork's tree after every merge; `README.md` and `.gitignore` are regenerated as upstream's version plus `.github/fork-overlay/{readme,gitignore}.append`, so upstream text lands verbatim and the fork's appended block cannot be lost or conflict; every other path takes upstream verbatim, deletions included.
- Consequence: the fork's `.github/` is frozen, so upstream CI improvements (e.g. new jobs in `ci.yml`) are not adopted. Edit the fork's copy deliberately if one is wanted. Fork content for `README.md` / `.gitignore` must be edited in `.github/fork-overlay/*.append`, not in the files themselves — the next sync overwrites them.
- `.github/workflows/ci.yml` and `clispec.yml` — upstream's Rust lint/test and CLI-conformance checks, retained.

## What was removed from upstream CI

| Removed | Why |
|---------|-----|
| `release.yml` | Published to crates.io / PyPI / `rvben/homebrew-tap`, needed three secrets, required self-hosted ARM64 runners this fork lacks |
| `upd.yml` | Scheduled dependency bumps would diverge the fork and conflict on every upstream sync |
| `ci.yml` `coverage` job | Needs `CODECOV_TOKEN` |
| `clispec.yml` `published-artifacts` job | Polled upstream's crates.io / PyPI / Homebrew artifacts |

## One-time setup

- Actions are already enabled on this fork; workflows register on first use. `flakehub.yml` and `auto-update.yml` only register once they land on the default branch.
- The published name is set explicitly in `.github/workflows/flakehub.yml` as `rzp/unifi-cli-nix`. The FlakeHub org `rzp` is already linked to this GitHub account (same setup as `rzp/den-lsp`), so no additional FlakeHub configuration is needed.
- Nothing else (no secrets, no tokens).
- `auto-update.yml` promotes to `main` with `GITHUB_TOKEN`, so enabling branch protection on `main` would require swapping in a PAT or GitHub App token.
- `GITHUB_TOKEN` pushes fire no `push` events, which is why `auto-update.yml` calls `flakehub.yml` directly instead of relying on `nix.yml`.

## Upstream contributions

Fixes to the CLI itself belong upstream at [`rvben/unifi-cli`](https://github.com/rvben/unifi-cli); only Nix packaging lives here.
