#!/usr/bin/env bash
# Plan offline, then publish an immutable tagged release only after verifying all assets.
set -euo pipefail
task_root="$(cd -- "$(dirname -- "$BASH_SOURCE")/.." && pwd)"
exec python3 - "$task_root" "$@" <<'PYTHON'
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import zipfile

parser = argparse.ArgumentParser(description="Validate a FlashTool release; publish draft assets before sending GitHub's published event.")
parser.add_argument("--root", type=Path, default=Path(sys.argv[1]))
parser.add_argument("--assets-dir", type=Path)
parser.add_argument("--repo", help="GitHub owner/repository; defaults to GH_REPO or the origin remote")
parser.add_argument("--tag", help="Must match v plus Cargo.toml's version")
parser.add_argument("--check", action="store_true", help="Print a completely offline release plan without writes")
parser.add_argument("--dry-run", action="store_true", help="Alias for --check")
args = parser.parse_args(sys.argv[2:])
root = args.root.resolve()
assets_dir = args.assets_dir.resolve() if args.assets_dir else root / "dist"

def run(command, *, ok=False):
    result = subprocess.run(command, cwd=root, capture_output=True, text=True)
    if result.returncode and not ok:
        raise RuntimeError(f"{command[0]} failed ({result.returncode}): {result.stderr.strip()[:1500]}")
    return result

def digest(data):
    return hashlib.sha256(data).hexdigest()

def gh(*arguments, missing_ok=False):
    result = run(["gh", *arguments], ok=True)
    if result.returncode:
        # Permission/network failures are never evidence that a release is absent.
        if missing_ok and re.search(r"\(HTTP 404\)", result.stderr):
            return None
        raise RuntimeError(f"GitHub operation failed ({result.returncode}): {result.stderr.strip()[:1500]}")
    return json.loads(result.stdout) if result.stdout.strip() else None

def api(endpoint, *, missing_ok=False):
    return gh("api", endpoint, missing_ok=missing_ok)

def stable_tag_version(value):
    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value or "")
    if not match:
        raise ValueError("Existing Latest release does not use a stable vMAJOR.MINOR.PATCH tag")
    return tuple(map(int, match.groups()))

try:
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    version = manifest["package"]["version"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Cargo.toml must declare a stable MAJOR.MINOR.PATCH version")
    tag = "v" + version
    if args.tag and args.tag != tag:
        raise ValueError(f"Requested tag {args.tag!r} does not match Cargo.toml {tag}")
    commit = run(["git", "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"]).stdout.strip()
    if not re.fullmatch(r"[0-9a-f]{40,64}", commit):
        raise ValueError("The local release tag did not resolve to a full commit")
    tagged_manifest = tomllib.loads(run(["git", "show", f"{commit}:Cargo.toml"]).stdout)
    if tagged_manifest["package"]["version"] != version:
        raise ValueError("The existing tag's Cargo.toml declares another version; tags are never moved")
    changelog = run(["git", "show", f"{commit}:CHANGELOG.md"]).stdout
    notes = re.search(r"^## \[" + re.escape(version) + r"\][^\n]*\n(.*?)(?=^## |\Z)", changelog, re.M | re.S)
    if not notes:
        raise ValueError("The tagged source is missing this version's CHANGELOG release notes")
    windows_name = f"FlashTool-{tag}-windows-x64.zip"
    source_name = f"FlashTool-{tag}-source.zip"
    windows_path = assets_dir / windows_name
    windows_bytes = windows_path.read_bytes()
    windows_hash = digest(windows_bytes)
    checksum_paths = [assets_dir / f"SHA256SUMS-{tag}", assets_dir / "SHA256SUMS", assets_dir / "SHA256SUMS.txt"]
    input_sums = next((path for path in checksum_paths if path.is_file()), None)
    if input_sums is None:
        raise ValueError("Missing input SHA256SUMS or versioned SHA256SUMS-vX.Y.Z")
    checksums = {}
    for line in input_sums.read_text(encoding="utf-8-sig").splitlines():
        match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *](.+)", line.strip())
        if not match:
            raise ValueError("The input checksum file contains a malformed line")
        if match[2] in checksums:
            raise ValueError("The input checksum file contains duplicate names")
        checksums[match[2]] = match[1].lower()
    if checksums.get(windows_name) != windows_hash:
        raise ValueError("The Windows ZIP does not match its input checksum")
    with zipfile.ZipFile(windows_path) as archive:
        metadata = json.loads(archive.read("BUILD-METADATA.json").decode("utf-8-sig"))
        if metadata.get("version") != version:
            raise ValueError("The Windows ZIP's embedded version does not match the release")
        if metadata.get("target") not in ("x86_64-pc-windows-msvc", "x86_64-pc-windows-gnu"):
            raise ValueError("The asset is not a Windows x64 GNU/MSVC build")
        if metadata.get("commit") and metadata["commit"] != commit:
            raise ValueError("The Windows ZIP records a different source commit")
        if archive.read("FlashTool.exe")[:2] != b"MZ":
            raise ValueError("The Windows ZIP does not contain a PE executable")
    # Archive the immutable tag, never the current working tree or private attachments.
    source_bytes = subprocess.run(
        ["git", "archive", "--format=zip", "--prefix", f"FlashTool-{tag}/", commit],
        cwd=root, capture_output=True, check=True
    ).stdout
    source_hash = digest(source_bytes)
    checksum_bytes = f"{windows_hash}  {windows_name}\n{source_hash}  {source_name}\n".encode("ascii")
    payloads = {windows_name: windows_bytes, source_name: source_bytes, "SHA256SUMS": checksum_bytes}
    expected_assets = {name: {"sha256": digest(data), "bytes": len(data)} for name, data in payloads.items()}
    repository = args.repo or os.environ.get("GH_REPO")
    if not repository:
        origin = run(["git", "remote", "get-url", "origin"]).stdout.strip()
        match = re.fullmatch(r"(?:https://(?:[^/@]+@)?github\.com/|git@github\.com:)([^/]+/[^/]+?)(?:\.git)?", origin)
        if not match:
            raise ValueError("Pass --repo owner/repository for a GitHub origin")
        repository = match[1]
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Invalid GitHub owner/repository")
    title = "FlashTool " + tag
    plan = {
        "repository": repository, "tag": tag, "title": title, "commit": commit,
        "assets": expected_assets, "target": metadata["target"],
        "driver_packages": metadata.get("driver_packages", {}),
        "steps": ["verify remote tag commit", "create or resume draft", "upload and verify every asset",
                  "publish draft; advance Latest only for a non-older stable SemVer", "verify published release and Latest"],
        "notification": "GitHub's published event notifies users who subscribed to Releases; email delivery depends on their GitHub settings.",
    }
    print(json.dumps(plan, ensure_ascii=False, indent=2), flush=True)
    if args.check or args.dry_run:
        sys.exit(0)

    prefix = "repos/" + repository
    def verify_remote_tag():
        obj = api(f"{prefix}/git/ref/tags/{tag}")["object"]
        for _ in range(8):
            if obj["type"] == "commit":
                if obj["sha"] != commit:
                    raise ValueError("Remote tag points at a different commit; no tag was changed")
                return
            if obj["type"] != "tag" or not re.fullmatch(r"[0-9a-f]{40,64}", obj["sha"]):
                break
            obj = api(f"{prefix}/git/tags/{obj['sha']}")["object"]
        raise ValueError("Could not resolve the remote tag to the expected commit")

    def validate_release(release):
        if release.get("tag_name") != tag or release.get("name") != title:
            raise ValueError("Existing release tag/title differs; it was not overwritten")
        target = release.get("target_commitish")
        if not isinstance(target, str) or not target:
            raise ValueError("Existing release has no target_commitish metadata")
        # For an existing tag GitHub may retain a branch name here and ignore
        # --target. The peeled remote tag is the authoritative source commit.
        if re.fullmatch(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})", target) and target.lower() != commit:
            raise ValueError("Existing release names a different explicit target SHA; it was not overwritten")
        if release.get("prerelease"):
            raise ValueError("A prerelease is not a stable FlashTool release")

    with tempfile.TemporaryDirectory(prefix="flashtool-publish-") as directory:
        temporary = Path(directory)
        for name, data in payloads.items():
            (temporary / name).write_bytes(data)
        notes_path = temporary / "release-notes.md"
        notes_text = notes[1].strip()
        notes_text += "\n\nWindows x64 构建平台和验证范围见 ZIP 内 BUILD-METADATA.json；USB Hub 多设备烧录及驱动安装仍需实机验收。"
        notes_text += "\n\n驱动包状态：" + json.dumps(metadata.get("driver_packages", {}), ensure_ascii=False)
        notes_text += "\n\n订阅仓库 Watch → Custom → Releases 可接收 GitHub 发布通知；邮件是否送达取决于个人通知设置。\n"
        notes_path.write_text(notes_text, encoding="utf-8")
        verify_remote_tag()
        release = api(f"{prefix}/releases/tags/{tag}", missing_ok=True)
        if release is None:
            run(["gh", "release", "create", tag, "--repo", repository, "--draft", "--verify-tag",
                 "--target", commit, "--title", title, "--notes-file", str(notes_path)])
            release = api(f"{prefix}/releases/tags/{tag}")
        validate_release(release)
        published = not release.get("draft", False)
        existing = {asset["name"]: asset for asset in release.get("assets", [])}
        for name in payloads:
            if name in existing:
                continue
            if published:
                raise ValueError(f"Published release is missing {name}; published assets are never changed")
            run(["gh", "release", "upload", tag, str(temporary / name), "--repo", repository])
        release = api(f"{prefix}/releases/tags/{tag}")
        validate_release(release)
        actual = {asset["name"]: asset for asset in release.get("assets", [])}
        for name, expected in expected_assets.items():
            asset = actual.get(name)
            if asset is None or asset.get("size") != expected["bytes"]:
                raise ValueError(f"Uploaded release asset missing or wrong size: {name}")
            checksum = asset.get("digest")
            if isinstance(checksum, str) and re.fullmatch(r"sha256:[0-9a-fA-F]{64}", checksum):
                actual_hash = checksum.split(":", 1)[1].lower()
            else:
                download = temporary / ("verify-" + name.replace(".", "_"))
                download.mkdir()
                run(["gh", "release", "download", tag, "--repo", repository, "--pattern", name, "--dir", str(download)])
                actual_hash = digest((download / name).read_bytes())
            if actual_hash != expected["sha256"]:
                raise ValueError(f"Release asset checksum mismatch; release remains unchanged/draft: {name}")
        verify_remote_tag()
        current_version = stable_tag_version(tag)
        previous_latest = api(f"{prefix}/releases/latest", missing_ok=True)
        previous_version = stable_tag_version(previous_latest.get("tag_name")) if previous_latest else None
        make_latest = previous_version is None or current_version >= previous_version
        if not published:
            latest_flag = "--latest" if make_latest else "--latest=false"
            run(["gh", "release", "edit", tag, "--repo", repository, "--draft=false", latest_flag])
        elif make_latest and (previous_latest is None or previous_latest.get("id") != release["id"]):
            run(["gh", "release", "edit", tag, "--repo", repository, "--latest"])
        final = api(f"{prefix}/releases/tags/{tag}")
        latest = api(f"{prefix}/releases/latest")
        validate_release(final)
        latest_version = stable_tag_version(latest.get("tag_name"))
        if final.get("draft") or latest_version < current_version:
            raise ValueError("GitHub did not confirm publication with a non-older stable Latest release")
        if previous_version is not None and latest_version < previous_version:
            raise ValueError("GitHub's Latest release unexpectedly regressed")
        if latest_version == current_version and latest.get("id") != final["id"]:
            raise ValueError("GitHub Latest has the same version but a different release identity")
        print("Published and verified: " + final.get("html_url", tag) + " (Latest: " + latest["tag_name"] + ")")
except (ValueError, KeyError, OSError, RuntimeError, subprocess.CalledProcessError, zipfile.BadZipFile) as error:
    print("Release failed: " + str(error), file=sys.stderr)
    sys.exit(1)
PYTHON
