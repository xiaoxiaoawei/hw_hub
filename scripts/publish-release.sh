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
from urllib.parse import urlencode
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
        raise RuntimeError(f"{' '.join(command[:3])} failed ({result.returncode}): {result.stderr.strip()[:1500]}")
    return result

def digest(data):
    return hashlib.sha256(data).hexdigest()

def gh(*arguments, missing_ok=False):
    result = run(["gh", *arguments], ok=True)
    if result.returncode:
        # Permission/network failures are never evidence that a release is absent.
        if missing_ok and re.search(r"\(HTTP 404\)", result.stderr):
            return None
        context = ' '.join(arguments[:2])
        raise RuntimeError(f"GitHub {context} failed ({result.returncode}): {result.stderr.strip()[:1500]}")
    return json.loads(result.stdout) if result.stdout.strip() else None

def api(endpoint, *, missing_ok=False):
    return gh("api", endpoint, missing_ok=missing_ok)

def stable_tag_version(value):
    match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value or "")
    if not match:
        raise ValueError("Existing Latest release does not use a stable vMAJOR.MINOR.PATCH tag")
    return tuple(map(int, match.groups()))

def safe_error_message(message):
    message = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", message)
    message = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]", "", message)
    message = re.sub(r"(https?://)[^/\s@]+@", r"\1[redacted]@", message, flags=re.I)
    message = re.sub(r"\b(?:gh[pousr]_|github_pat_)[A-Za-z0-9_]+", "[redacted-token]", message)
    message = re.sub(r"\b(authorization|password|secret|(?:api[_-]?)?token|api[_-]?key)\s*[:=]\s*\S+", r"\1=[redacted]", message, flags=re.I)
    if len(message) > 6000:
        message = message[:2000] + "\n[bounded diagnostic]\n" + message[-3900:]
    return message

def github_diagnostic(message):
    return safe_error_message(message).replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")

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
    # Preserve already-published legacy archive bytes. New versions package only
    # the immutable commit's development inputs, not the legacy root installer.
    archive_command = ["git", "archive", "--format=zip", "--prefix", f"FlashTool-{tag}/", commit]
    source_policy = "legacy-full-tree"
    if stable_tag_version(tag) >= (0, 4, 2):
        source_policy = "build-source-allowlist"
        allowed = {
            "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "build.rs", "README.md", "CHANGELOG.md",
            ".gitattributes", ".gitignore", "src", "assets", "scripts", "tests", ".github",
        }
        entries = subprocess.run(["git", "ls-tree", "-z", commit], cwd=root, capture_output=True, check=True).stdout
        paths = []
        for entry in entries.split(b"\0"):
            if not entry:
                continue
            attributes, encoded_name = entry.split(b"\t", 1)
            name = encoded_name.decode("utf-8")
            is_notice = attributes.split()[1] == b"blob" and re.match(r"^(LICENSE|COPYING|NOTICE|THIRD_PARTY)", name)
            if name in allowed or is_notice:
                paths.append(name)
        if "Cargo.toml" not in paths or "src" not in paths:
            raise ValueError("The tagged source is missing required build inputs")
        archive_command += ["--", *sorted(paths)]
    source_bytes = subprocess.run(
        archive_command, cwd=root, capture_output=True, check=True
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
        "source_archive_policy": source_policy,
        "steps": ["verify remote tag commit", "create or resume draft", "upload and verify every asset",
                  "publish draft; advance Latest only for a non-older stable SemVer", "verify published release and Latest"],
        "build_notifications": "GitHub Actions completion emails follow the triggering user's Actions Email preferences, including the only-failed setting.",
    }
    print(json.dumps(plan, ensure_ascii=False, indent=2), flush=True)
    if args.check or args.dry_run:
        sys.exit(0)

    prefix = "repos/" + repository
    def find_release():
        # GitHub's tag endpoint only finds published releases. An authenticated
        # list includes drafts; fetch a matching draft by its immutable ID.
        found = api(f"{prefix}/releases/tags/{tag}", missing_ok=True)
        if found is not None:
            return found
        for page in range(1, 101):
            releases = api(f"{prefix}/releases?per_page=100&page={page}")
            if not isinstance(releases, list):
                raise ValueError("GitHub release listing did not return an array")
            matches = [item for item in releases if item.get("tag_name") == tag]
            if len(matches) > 1:
                raise ValueError("Multiple releases use the requested tag; no draft was selected")
            if matches:
                return api(f"{prefix}/releases/{matches[0]['id']}")
            if len(releases) < 100:
                return None
        raise ValueError("Release lookup exceeded 100 pages; absence is unconfirmed")

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
        notes_text = notes[1].strip()
        notes_text += "\n\nWindows x64 构建平台和验证范围见 ZIP 内 BUILD-METADATA.json；USB Hub 多设备烧录及驱动安装仍需实机验收。"
        notes_text += "\n\n驱动包状态：" + json.dumps(metadata.get("driver_packages", {}), ensure_ascii=False)
        verify_remote_tag()
        release = find_release()
        if release is None:
            request_path = temporary / "create-release.json"
            request_path.write_text(json.dumps({
                "tag_name": tag, "target_commitish": commit, "name": title,
                "body": notes_text, "draft": True, "prerelease": False,
            }, ensure_ascii=False), encoding="utf-8")
            # The POST response is authoritative immediately. Draft tag/list
            # indexes can lag and must not be consulted again to rediscover it.
            release = gh("api", f"{prefix}/releases", "--method", "POST", "--input", str(request_path))
            if not isinstance(release, dict) or release.get("draft") is not True:
                raise ValueError("GitHub did not return the newly created draft release object")
        validate_release(release)
        release_id = release.get("id")
        if not isinstance(release_id, int) or release_id <= 0:
            raise ValueError("GitHub did not return a valid release ID")
        release_endpoint = f"{prefix}/releases/{release_id}"
        published = not release.get("draft", False)
        existing = {asset["name"]: asset for asset in release.get("assets", [])}
        for name in payloads:
            if name in existing:
                continue
            if published:
                raise ValueError(f"Published release is missing {name}; published assets are never changed")
            upload_endpoint = f"https://uploads.github.com/repos/{repository}/releases/{release_id}/assets?" + urlencode({"name": name})
            gh("api", upload_endpoint, "--method", "POST", "--header", "Content-Type: application/octet-stream",
               "--header", f"Content-Length: {len(payloads[name])}", "--input", str(temporary / name))
        release = api(release_endpoint)
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
                asset_id = asset.get("id")
                if not isinstance(asset_id, int) or asset_id <= 0:
                    raise ValueError(f"GitHub did not return a valid asset ID: {name}")
                download = subprocess.run(["gh", "api", f"{prefix}/releases/assets/{asset_id}", "--header", "Accept: application/octet-stream"],
                                          cwd=root, capture_output=True)
                if download.returncode:
                    raise RuntimeError(f"GitHub asset download failed ({download.returncode}): " + download.stderr.decode("utf-8", errors="replace")[:1500])
                actual_hash = digest(download.stdout)
            if actual_hash != expected["sha256"]:
                raise ValueError(f"Release asset checksum mismatch; release remains unchanged/draft: {name}")
        verify_remote_tag()
        current_version = stable_tag_version(tag)
        previous_latest = api(f"{prefix}/releases/latest", missing_ok=True)
        previous_version = stable_tag_version(previous_latest.get("tag_name")) if previous_latest else None
        make_latest = previous_version is None or current_version >= previous_version
        patch = None
        if not published:
            patch = {"draft": False, "make_latest": "true" if make_latest else "false"}
        elif make_latest and (previous_latest is None or previous_latest.get("id") != release["id"]):
            patch = {"make_latest": "true"}
        if patch is not None:
            patch_path = temporary / "edit-release.json"
            patch_path.write_text(json.dumps(patch), encoding="utf-8")
            gh("api", release_endpoint, "--method", "PATCH", "--input", str(patch_path))
        final = api(release_endpoint)
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
    detail = str(error)
    if isinstance(error, subprocess.CalledProcessError) and error.stderr:
        detail += "\n" + (error.stderr.decode("utf-8", errors="replace") if isinstance(error.stderr, bytes) else error.stderr)
    detail = safe_error_message(detail)
    if os.environ.get("GITHUB_ACTIONS", "").lower() == "true":
        print("::error title=FlashTool release failed::" + github_diagnostic(detail), flush=True)
    print("Release failed: " + detail, file=sys.stderr)
    sys.exit(1)
PYTHON
