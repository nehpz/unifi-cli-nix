#!/usr/bin/env python3
"""Install and test the generated formula using native binaries or release artifacts.

The binary mode uses the host binaries for all four archive fixtures; only the
native platform is installed. Artifact mode validates every real release archive.
An existing Homebrew installation is never replaced.
"""

import argparse
import hashlib
import os
import shutil
import subprocess
import tarfile
import tempfile
import uuid
from pathlib import Path

import tomllib
from homebrew_formula import TARGETS, render


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sources = parser.add_mutually_exclusive_group(required=True)
    sources.add_argument("--bin-dir", type=Path)
    sources.add_argument("--artifacts", type=Path)
    args = parser.parse_args()
    env = dict(
        os.environ,
        HOMEBREW_NO_AUTO_UPDATE="1",
        HOMEBREW_NO_INSTALL_CLEANUP="1",
        HOMEBREW_NO_AUTOREMOVE="1",
        HOMEBREW_NO_ANALYTICS="1",
    )

    def brew(*arguments, **kwargs):
        return subprocess.run(["brew", *arguments], env=env, check=True, **kwargs)

    installed = brew(
        "list", "--formula", "--versions", capture_output=True, text=True
    ).stdout
    if any(line.split()[0] == "unifi-cli" for line in installed.splitlines()):
        parser.error("unifi-cli is already installed; use a clean Homebrew runner")

    root = Path(__file__).resolve().parent.parent
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    with tempfile.TemporaryDirectory(prefix="unifi-homebrew-") as directory:
        directory = Path(directory).resolve()
        env["XDG_CONFIG_HOME"] = str(directory / "config")
        if args.bin_dir:
            for target in TARGETS:
                archive = directory / f"unifi-cli-v{version}-{target}.tar.gz"
                with tarfile.open(archive, "w:gz") as package:
                    for command in ("unifi", "unifi-cli"):
                        package.add(args.bin_dir / command, arcname=command)
                Path(f"{archive}.sha256").write_text(
                    hashlib.sha256(archive.read_bytes()).hexdigest()
                )
        else:
            for path in args.artifacts.rglob("unifi-cli-*.tar.gz*"):
                destination = directory / path.name
                if destination.exists():
                    parser.error(f"duplicate release artifact: {path.name}")
                shutil.copyfile(path, destination)
        rendered = render(version, directory, directory.as_uri())
        tap = f"local/unifi-smoke-{uuid.uuid4().hex[:12]}"
        brew("tap-new", "--no-git", tap)
        try:
            tap_path = Path(
                brew("--repository", tap, capture_output=True, text=True).stdout.strip()
            )
            formula = tap_path / "Formula/unifi-cli.rb"
            formula.write_text(rendered)
            name = f"{tap}/unifi-cli"
            try:
                brew("install", "--formula", "--build-from-source", "--skip-link", name)
                brew("test", "--force", name)
                prefix = Path(
                    brew(
                        "--prefix", name, capture_output=True, text=True
                    ).stdout.strip()
                )
                for completion in (
                    "etc/bash_completion.d/unifi",
                    "share/zsh/site-functions/_unifi",
                    "share/fish/vendor_completions.d/unifi.fish",
                ):
                    path = prefix / completion
                    if not path.is_file() or not path.stat().st_size:
                        raise RuntimeError(f"missing shell completion: {path}")
            finally:
                # The guard above ensures this only removes our test installation.
                installed = brew(
                    "list", "--formula", "--versions", capture_output=True, text=True
                ).stdout
                if any(
                    line.split()[0] == "unifi-cli" for line in installed.splitlines()
                ):
                    brew("uninstall", "--force", name)
        finally:
            brew("untap", tap)


if __name__ == "__main__":
    main()
