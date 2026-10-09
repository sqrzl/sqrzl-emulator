"""Independent negative checks for the SDK artifact provenance boundary."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import conftest
from build_provenance import validate_provenance


class ProvenanceChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name)
        (self.repo / "sdk-tests").mkdir()
        helper = Path(__file__).with_name("build_provenance.py").read_bytes()
        (self.repo / "sdk-tests/build_provenance.py").write_bytes(helper)
        (self.repo / "Cargo.lock").write_text("synthetic tracked lock\n")
        for command in [
            ["git", "init", "-q"],
            ["git", "add", "."],
            [
                "git",
                "-c",
                "user.name=Provenance test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "synthetic source",
            ],
        ]:
            subprocess.run(command, cwd=self.repo, check=True)
        commit = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=self.repo, text=True
        ).strip()
        tree = subprocess.check_output(
            ["git", "rev-parse", "HEAD^{tree}"], cwd=self.repo, text=True
        ).strip()
        snapshot = {"commit": commit, "tree": tree, "clean": True}
        self.binary = self.repo / "binary"
        self.binary.write_bytes(b"synthetic executable bytes")
        self.manifest = self.repo / "provenance.json"
        self.provenance = {
            "schema_version": 1,
            "kind": "sqrzl-qualification-build",
            "source_commit": commit,
            "source_tree": tree,
            "source_before": snapshot,
            "source_after": snapshot,
            "source_verified": True,
            "build_command": [
                "cargo",
                "build",
                "--locked",
                "--bin",
                "sqrzl-emulator",
                "--target-dir",
                str(self.repo / "target"),
            ],
            "build_profile": "debug",
            "binary_sha256": hashlib.sha256(self.binary.read_bytes()).hexdigest(),
            "cargo_lock_sha256": hashlib.sha256(
                (self.repo / "Cargo.lock").read_bytes()
            ).hexdigest(),
            "build_helper_sha256": hashlib.sha256(helper).hexdigest(),
        }

    def verify(self):
        self.manifest.write_text(json.dumps(self.provenance))
        return validate_provenance(self.binary, self.manifest, repo=self.repo)

    def test_matching_clean_source_and_binary_is_verified(self):
        self.assertEqual(self.verify(), self.provenance)

    def test_binary_substitution_is_rejected(self):
        self.binary.write_bytes(b"different executable")
        with self.assertRaisesRegex(ValueError, "binary digest"):
            self.verify()

    def test_dirty_or_changed_build_is_rejected(self):
        self.provenance["source_after"] = {
            **self.provenance["source_after"],
            "clean": False,
        }
        with self.assertRaisesRegex(ValueError, "stable clean"):
            self.verify()

    def test_plain_source_sha_is_insufficient(self):
        self.provenance = {"source_commit": self.provenance["source_commit"]}
        with self.assertRaisesRegex(ValueError, "unsupported"):
            self.verify()

    def test_untracked_helper_or_lock_is_rejected(self):
        for field in ["cargo_lock_sha256", "build_helper_sha256"]:
            with self.subTest(field=field):
                original = self.provenance[field]
                self.provenance[field] = "0" * 64
                with self.assertRaisesRegex(ValueError, field):
                    self.verify()
                self.provenance[field] = original

    def test_bare_external_override_cannot_claim_source_head(self):
        with patch.dict(os.environ, {"SQRZL_BINARY": str(self.binary)}, clear=True):
            copied = conftest._ensure_binary(self.repo / "runtime-bin")
        self.assertEqual(copied.read_bytes(), self.binary.read_bytes())
        self.assertFalse(conftest._BINARY_PROVENANCE["binary_source_verified"])
        self.assertIsNone(conftest._BINARY_PROVENANCE["binary_source_commit"])
        self.assertIsNone(conftest._BINARY_PROVENANCE["build_provenance"])


if __name__ == "__main__":
    unittest.main()
