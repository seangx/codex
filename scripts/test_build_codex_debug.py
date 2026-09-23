"""Regression checks for the debug installer's advertised client version."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class DebugBuildVersionTests(unittest.TestCase):
    def resolve_version(self, official_version, model_versions):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            shutil.copyfile(
                Path(__file__).with_name("build-codex-debug.sh"),
                scripts / "build-codex-debug.sh",
            )
            catalog_dir = root / "codex-rs" / "models-manager"
            catalog_dir.mkdir(parents=True)
            (root / "codex-rs" / "Cargo.toml").write_text("", encoding="utf-8")
            (root / "codex-rs" / "Cargo.lock").write_text("", encoding="utf-8")
            (catalog_dir / "models.json").write_text(
                json.dumps(
                    {"models": [{"minimal_client_version": v} for v in model_versions]}
                ),
                encoding="utf-8",
            )
            official = root / "official-codex"
            official.write_text(
                f"#!/bin/sh\nprintf '%s\\n' 'codex-cli {official_version}'\n",
                encoding="utf-8",
            )
            official.chmod(0o755)
            for args in [
                ["init", "-q"],
                ["add", "."],
                [
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-qm",
                    "fixture",
                ],
            ]:
                subprocess.run(
                    ["git", "-C", str(root), *args], check=True, capture_output=True
                )
            sha = subprocess.check_output(
                ["git", "-C", str(root), "rev-parse", "--short=10", "HEAD"], text=True
            ).strip()
            result = subprocess.run(
                ["sh", str(scripts / "build-codex-debug.sh"), "--print-version"],
                env={**os.environ, "CODEX_OFFICIAL_BIN": str(official)},
                capture_output=True,
                text=True,
            )
            return result, sha

    def test_source_catalog_raises_stale_installed_version(self):
        result, sha = self.resolve_version("0.153.2", ["0.98.0", "0.155.0", None])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"0.155.0+debug.{sha}\n")

    def test_newer_official_version_is_preserved(self):
        result, sha = self.resolve_version("0.156.1", ["0.155.0"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"0.156.1+debug.{sha}\n")

    def test_missing_catalog_requirements_keep_official_baseline(self):
        result, sha = self.resolve_version("0.153.2", [None])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"0.153.2+debug.{sha}\n")

    def test_malformed_catalog_requirement_stops_installation(self):
        result, _ = self.resolve_version("0.153.2", ["not-a-version"])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid model catalog client version", result.stderr)
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
