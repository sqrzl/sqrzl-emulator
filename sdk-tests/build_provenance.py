"""Build an immutable SDK qualification binary with checked source provenance."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
from datetime import datetime, timezone
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
HELPER_PATH = "sdk-tests/build_provenance.py"


def _git(repo: Path, *args: str) -> str:
    return subprocess.check_output(["git", *args], cwd=repo, text=True).strip()


def _digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _snapshot(repo: Path) -> dict:
    return {
        "commit": _git(repo, "rev-parse", "HEAD"),
        "tree": _git(repo, "rev-parse", "HEAD^{tree}"),
        "clean": not bool(_git(repo, "status", "--porcelain")),
    }


def _tracked_digest(repo: Path, commit: str, path: str) -> str:
    data = subprocess.check_output(["git", "show", f"{commit}:{path}"], cwd=repo)
    return hashlib.sha256(data).hexdigest()


def build_verified_binary(
    output_dir: Path, *, allow_dirty: bool = False
) -> tuple[Path, dict]:
    before = _snapshot(REPO_ROOT)
    if not before["clean"] and not allow_dirty:
        raise RuntimeError("qualification build requires a clean source worktree")
    command = [
        "cargo",
        "build",
        "--locked",
        "--bin",
        "sqrzl-emulator",
        "--target-dir",
        str(REPO_ROOT / "target"),
    ]
    subprocess.run(command, cwd=REPO_ROOT, check=True)
    after = _snapshot(REPO_ROOT)
    output_dir.mkdir(parents=True, exist_ok=True)
    binary = output_dir / "sqrzl-emulator"
    if binary.exists() or (output_dir / "build-provenance.json").exists():
        raise RuntimeError(
            "qualification build output must be a new immutable artifact"
        )
    shutil.copyfile(REPO_ROOT / "target/debug/sqrzl-emulator", binary)
    binary.chmod(0o555)
    verified = before == after and before["clean"]
    provenance = {
        "schema_version": 1,
        "kind": "sqrzl-qualification-build",
        "source_commit": before["commit"],
        "source_tree": before["tree"],
        "source_before": before,
        "source_after": after,
        "source_verified": verified,
        "build_command": command,
        "build_profile": "debug",
        "cargo_lock_sha256": _digest(REPO_ROOT / "Cargo.lock"),
        "build_helper_sha256": _digest(Path(__file__)),
        "binary_sha256": _digest(binary),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "cargo": subprocess.check_output(["cargo", "--version"], text=True).strip(),
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "build_environment": {
            name: os.environ.get(name)
            for name in [
                "RUSTFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTC",
                "RUSTC_WRAPPER",
                "CARGO_BUILD_TARGET",
            ]
        },
    }
    (output_dir / "build-provenance.json").write_text(
        json.dumps(provenance, indent=2) + "\n"
    )
    return binary, provenance


def validate_provenance(
    binary: Path, manifest: Path, *, repo: Path = REPO_ROOT
) -> dict:
    provenance = json.loads(manifest.read_text())
    if (
        provenance.get("schema_version") != 1
        or provenance.get("kind") != "sqrzl-qualification-build"
    ):
        raise ValueError("unsupported qualification build provenance")
    commit = provenance.get("source_commit", "")
    # Resolve only an exact commit object, not an option or caller-provided symbolic ref.
    if len(commit) != 40 or any(c not in "0123456789abcdef" for c in commit):
        raise ValueError("build provenance requires an exact source commit")
    snapshot = {
        "commit": commit,
        "tree": _git(repo, "rev-parse", f"{commit}^{{tree}}"),
        "clean": True,
    }
    if (
        not provenance.get("source_verified")
        or provenance.get("source_before") != snapshot
        or provenance.get("source_after") != snapshot
        or provenance.get("source_tree") != snapshot["tree"]
    ):
        raise ValueError("build provenance does not bind a stable clean source tree")
    if provenance.get("binary_sha256") != _digest(binary):
        raise ValueError("binary digest does not match build provenance")
    for field, path in [
        ("cargo_lock_sha256", "Cargo.lock"),
        ("build_helper_sha256", HELPER_PATH),
    ]:
        if provenance.get(field) != _tracked_digest(repo, commit, path):
            raise ValueError(f"{field} does not match the declared source commit")
    command = provenance.get("build_command", [])
    if (
        len(command) != 7
        or command[:6]
        != ["cargo", "build", "--locked", "--bin", "sqrzl-emulator", "--target-dir"]
        or provenance.get("build_profile") != "debug"
    ):
        # Keep the builder format explicit; a manually supplied source SHA is insufficient.
        raise ValueError("unexpected qualification build command")
    return provenance


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()
    binary, provenance = build_verified_binary(args.output_dir.resolve())
    validate_provenance(binary, args.output_dir.resolve() / "build-provenance.json")
    print(
        json.dumps(
            {
                "binary": str(binary),
                "provenance": str(args.output_dir.resolve() / "build-provenance.json"),
                "source_commit": provenance["source_commit"],
                "binary_sha256": provenance["binary_sha256"],
            }
        )
    )


if __name__ == "__main__":
    main()
