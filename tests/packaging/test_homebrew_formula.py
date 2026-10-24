import hashlib
import importlib.util
import io
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "homebrew_formula", ROOT / "scripts/homebrew_formula.py"
)
formula = importlib.util.module_from_spec(spec)
spec.loader.exec_module(formula)


class FormulaTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.artifacts = Path(self.temp.name)
        for target in formula.TARGETS:
            self.archive(target)

    def archive(
        self, target, commands=("unifi", "unifi-cli"), mode=0o755, symlink=False
    ):
        directory = self.artifacts / f"release-{target}"
        directory.mkdir(exist_ok=True)
        path = directory / f"unifi-cli-v1.2.3-{target}.tar.gz"
        with tarfile.open(path, "w:gz") as package:
            for command in commands:
                content = b"#!/bin/sh\necho unifi 1.2.3\n"
                member = tarfile.TarInfo(command)
                member.size = len(content)
                member.mode = mode
                if symlink:
                    member.type = tarfile.SYMTYPE
                    member.linkname = "another-binary"
                    member.size = 0
                    package.addfile(member)
                else:
                    package.addfile(member, io.BytesIO(content))
        Path(f"{path}.sha256").write_text(hashlib.sha256(path.read_bytes()).hexdigest())
        return path

    def test_renders_release_urls_and_actual_checksums(self):
        rendered = formula.render("v1.2.3", self.artifacts)
        self.assertNotIn("@", rendered)
        for target in formula.TARGETS:
            archive = next(self.artifacts.rglob(f"*-{target}.tar.gz"))
            self.assertIn(hashlib.sha256(archive.read_bytes()).hexdigest(), rendered)
            self.assertIn(
                f"releases/download/v1.2.3/unifi-cli-v1.2.3-{target}.tar.gz", rendered
            )

    def test_rejects_archive_containing_only_launcher(self):
        self.archive(formula.TARGETS[0], commands=("unifi-cli",))
        with self.assertRaisesRegex(ValueError, "missing regular executable unifi$"):
            formula.render("1.2.3", self.artifacts)

    def test_rejects_missing_launcher(self):
        self.archive(formula.TARGETS[0], commands=("unifi",))
        with self.assertRaisesRegex(
            ValueError, "missing regular executable unifi-cli$"
        ):
            formula.render("1.2.3", self.artifacts)

    def test_rejects_wrong_checksum(self):
        archive = next(self.artifacts.rglob("*.tar.gz"))
        Path(f"{archive}.sha256").write_text("0" * 64)
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            formula.render("1.2.3", self.artifacts)

    def test_rejects_non_executable_binary(self):
        self.archive(formula.TARGETS[0], mode=0o644)
        with self.assertRaisesRegex(ValueError, "missing regular executable"):
            formula.render("1.2.3", self.artifacts)

    def test_rejects_symlink_instead_of_binary(self):
        self.archive(formula.TARGETS[0], symlink=True)
        with self.assertRaisesRegex(ValueError, "missing regular executable"):
            formula.render("1.2.3", self.artifacts)

    def test_prerelease_does_not_block_publication(self):
        for path in self.artifacts.rglob("*.tar.gz*"):
            path.rename(path.with_name(path.name.replace("1.2.3", "1.2.3-rc.1")))
        rendered = formula.render("v1.2.3-rc.1", self.artifacts)
        self.assertIn('version "1.2.3-rc.1"', rendered)
        self.assertIn("releases/download/v1.2.3-rc.1/", rendered)

    def test_rejects_missing_platform(self):
        next(self.artifacts.rglob("*.tar.gz")).unlink()
        with self.assertRaisesRegex(ValueError, "expected exactly one archive"):
            formula.render("1.2.3", self.artifacts)

    def test_rejects_duplicate_platform(self):
        archive = next(self.artifacts.rglob("*.tar.gz"))
        (self.artifacts / archive.name).write_bytes(archive.read_bytes())
        with self.assertRaisesRegex(ValueError, "expected exactly one archive"):
            formula.render("1.2.3", self.artifacts)


if __name__ == "__main__":
    unittest.main()
