#!/usr/bin/env python3
"""Render the tap formula only after validating all four release archives."""

import argparse
import hashlib
import re
import tarfile
from pathlib import Path

TARGETS = (
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
)
TEMPLATE = Path(__file__).resolve().parent.parent / "packaging/unifi-cli.rb.in"


def render(version, artifacts, base_url=None):
    version = version.removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+([+-][0-9A-Za-z.-]+)?", version):
        raise ValueError("expected a release version (X.Y.Z with optional suffix)")
    base_url = base_url or (
        f"https://github.com/rvben/unifi-cli/releases/download/v{version}"
    )
    if any(char in base_url for char in ('"', "\\", "#", "\n", "\r")):
        raise ValueError("unsafe download base URL")
    formula = TEMPLATE.read_text().replace("@VERSION@", version)
    formula = formula.replace("@BASE_URL@", base_url.rstrip("/"))
    for target in TARGETS:
        name = f"unifi-cli-v{version}-{target}.tar.gz"
        matches = list(Path(artifacts).rglob(name))
        if len(matches) != 1:
            raise ValueError(
                f"expected exactly one archive for {target}, found {len(matches)}"
            )
        archive = matches[0]
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        recorded = Path(f"{archive}.sha256").read_text().split()[0].lower()
        if digest != recorded:
            raise ValueError(f"checksum mismatch: {name}")
        with tarfile.open(archive) as package:
            members = package.getmembers()
            for command in ("unifi", "unifi-cli"):
                binaries = [member for member in members if member.name == command]
                if (
                    len(binaries) != 1
                    or not binaries[0].isfile()
                    or not binaries[0].mode & 0o111
                ):
                    raise ValueError(f"{name}: missing regular executable {command}")
        formula = formula.replace(f"@{target}@", digest)
    return formula


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--base-url", help="Override downloads for a local installation test"
    )
    args = parser.parse_args()
    try:
        formula = render(args.version, args.artifacts, args.base_url)
    except (ValueError, OSError, tarfile.TarError, IndexError) as error:
        parser.error(str(error))
    args.output.write_text(formula)


if __name__ == "__main__":
    main()
