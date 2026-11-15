#!/usr/bin/env python3
"""Verify a preview against a successful main-branch Windows workflow run."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import zipfile


def verify(directory, version, run_id):
    if not re.fullmatch(r"v\d+\.\d+\.\d+-preview\.\d+", version) or not run_id.isdigit():
        raise ValueError("Invalid preview version or workflow run ID")
    run = json.loads(subprocess.check_output([
        "gh", "api", f"repos/SKYRPG8957/Sculith/actions/runs/{run_id}",
    ], text=True, encoding="utf-8"))
    if (run["status"], run["conclusion"], run["event"], run["head_branch"], run["path"]) != (
        "completed", "success", "push", "main", ".github/workflows/windows-preview.yml",
    ):
        raise ValueError("Only successful Windows builds pushed to main can be published")
    commit = run["head_sha"]
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Invalid build source revision")
    tag = json.loads(subprocess.check_output([
        "gh", "api", f"repos/SKYRPG8957/Sculith/git/ref/tags/{version}",
    ], text=True, encoding="utf-8"))["object"]
    for _ in range(4):
        if tag["type"] == "commit":
            break
        if tag["type"] != "tag" or not re.fullmatch(r"[0-9a-f]{40}", tag["sha"]):
            raise ValueError("Invalid release tag object")
        tag = json.loads(subprocess.check_output([
            "gh", "api", f"repos/SKYRPG8957/Sculith/git/tags/{tag['sha']}",
        ], text=True, encoding="utf-8"))["object"]
    if tag["type"] != "commit" or tag["sha"] != commit:
        raise ValueError("The existing release tag must point to the tested source revision")
    name = f"sculith-{version}-windows-x86_64.zip"
    checksum_lines = (directory / "SHA256SUMS.txt").read_text(encoding="ascii").splitlines()
    if len(checksum_lines) != 1:
        raise ValueError("Expected exactly one archive checksum")
    expected, checked_name = checksum_lines[0].split("  ")
    with (directory / name).open("rb") as stream:
        actual = hashlib.file_digest(stream, "sha256").hexdigest()
    if checked_name != name or expected != actual:
        raise ValueError("Archive checksum or name mismatch")
    with zipfile.ZipFile(directory / name) as bundle:
        if bundle.testzip() is not None:
            raise ValueError("Corrupt release archive")
        metadata = json.loads(bundle.read("release.json"))
        if metadata != {
            "version": version, "commit": commit, "target": "x86_64-pc-windows-msvc",
            "stage": "development-preview", "official_client_verified": False,
        }:
            raise ValueError("Archive metadata does not match the tested source and version")
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            stream.write(f"commit={commit}\narchive={name}\n")
    print(f"Verified {version}, source {commit}, Windows workflow {run_id}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    verify(args.directory, args.version, args.run_id)
