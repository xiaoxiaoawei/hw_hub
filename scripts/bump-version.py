#!/usr/bin/env python3
"""Update the package version offline. Cargo.toml is the only version source.

Write the target release notes in CHANGELOG.md first, then run:
    python scripts/bump-version.py 0.3.0
    python scripts/bump-version.py --check

Requires Python 3.11+. Each file replacement is atomic; a runtime error rolls
back earlier replacements. No Git command or network operation is performed.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
import tomllib


SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
VERSION_LINE = re.compile(
    r'''(?m)^([ \t]*version[ \t]*=[ \t]*)(?P<quote>["'])(?P<value>[^"'\r\n]+)(?P=quote)(?P<tail>[^\r\n]*)(?P<cr>\r?)$'''
)
PACKAGE_TABLE = re.compile(r"(?ms)^\[package\][ \t]*(?:#[^\r\n]*)?\r?\n.*?(?=^\[|\Z)")
LOCK_TABLE = re.compile(r"(?ms)^\[\[package\]\][ \t]*\r?\n.*?(?=^\[\[package\]\]|\Z)")
RELEASE_HEADING = re.compile(r"(?m)^##[ \t]+\[([0-9]+\.[0-9]+\.[0-9]+)\](?:[ \t][^\r\n]*)?\r?$")


def semver(value: str) -> tuple[int, int, int]:
    match = SEMVER.fullmatch(value)
    if not match:
        raise ValueError(f"Invalid version {value!r}; use MAJOR.MINOR.PATCH without prefixes or prereleases")
    return tuple(map(int, match.groups()))


def root_package(lock: dict, name: str) -> dict:
    matches = [p for p in lock.get("package", []) if p.get("name") == name and "source" not in p]
    if len(matches) != 1:
        raise ValueError(f"Cargo.lock must contain exactly one local package named {name!r}")
    return matches[0]


def release_headings(changelog: str) -> set[str]:
    versions = RELEASE_HEADING.findall(changelog)
    if len(versions) != len(set(versions)):
        raise ValueError("CHANGELOG.md contains duplicate release headings")
    return set(versions)


def replace_version_line(table: str, old: str, new: str) -> str:
    matches = list(VERSION_LINE.finditer(table))
    if len(matches) != 1 or matches[0].group("value") != old:
        raise ValueError("Expected exactly one matching package version assignment")
    match = matches[0]
    value = match.group(1) + match.group("quote") + new + match.group("quote") + match.group("tail") + match.group("cr")
    return table[: match.start()] + value + table[match.end() :]


def replace_manifest(text: str, old: str, new: str) -> str:
    matches = list(PACKAGE_TABLE.finditer(text))
    if len(matches) != 1:
        raise ValueError("Cargo.toml must have a single explicit [package] table")
    match = matches[0]
    return text[: match.start()] + replace_version_line(match.group(), old, new) + text[match.end() :]


def replace_lock(text: str, name: str, old: str, new: str) -> str:
    matches = []
    for match in LOCK_TABLE.finditer(text):
        package = tomllib.loads(match.group())["package"][0]
        if package.get("name") == name and "source" not in package:
            matches.append(match)
    if len(matches) != 1:
        raise ValueError("Could not uniquely locate the local package in Cargo.lock")
    match = matches[0]
    return text[: match.start()] + replace_version_line(match.group(), old, new) + text[match.end() :]


def stage_file(path: Path, content: bytes) -> Path:
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.version-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, stat.S_IMODE(path.stat().st_mode))
        return Path(temporary)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise


def atomic_update(changes: dict[Path, bytes], originals: dict[Path, bytes]) -> None:
    staged: dict[Path, Path] = {}
    replaced: list[Path] = []
    try:
        for path, content in changes.items():
            staged[path] = stage_file(path, content)
        for path in changes:
            if path.is_symlink() or path.read_bytes() != originals[path]:
                raise ValueError(f"{path.name} changed during the update; no files were replaced")
        for path, temporary in staged.items():
            os.replace(temporary, path)
            replaced.append(path)
    except BaseException:
        for path in reversed(replaced):
            recovery = stage_file(path, originals[path])
            try:
                os.replace(recovery, path)
            except BaseException as error:
                # Retain the recovery copy if the filesystem will not accept rollback.
                raise RuntimeError(f"Rollback failed; original content retained at {recovery}") from error
        raise
    finally:
        for temporary in staged.values():
            temporary.unlink(missing_ok=True)


def update(root: Path, target: str | None, *, check: bool = False, dry_run: bool = False) -> str:
    paths = [root / name for name in ("Cargo.toml", "Cargo.lock", "CHANGELOG.md")]
    for path in paths:
        if path.is_symlink():
            raise ValueError(f"Refusing to modify a symlink: {path.name}")
    originals = {path: path.read_bytes() for path in paths}
    manifest, lock, changelog = (originals[path].decode("utf-8") for path in paths)
    package = tomllib.loads(manifest)["package"]
    name, current = package["name"], package["version"]
    current_tuple = semver(current)
    if root_package(tomllib.loads(lock), name)["version"] != current:
        raise ValueError("Cargo.toml and the local package in Cargo.lock have different versions")
    headings = release_headings(changelog)
    if current not in headings:
        raise ValueError(f"CHANGELOG.md is missing '## [{current}]'")
    if check:
        if target is not None and (semver(target) != current_tuple):
            raise ValueError(f"Expected version {target}, but Cargo.toml declares {current}")
        return current
    if target is None:
        raise ValueError("Specify a target version, or use --check")
    if semver(target) <= current_tuple:
        raise ValueError("The new version must be greater than the current version; duplicate releases are refused")
    if target not in headings:
        raise ValueError(f"Write release notes under '## [{target}]' in CHANGELOG.md before bumping")
    updated_manifest = replace_manifest(manifest, current, target)
    updated_lock = replace_lock(lock, name, current, target)
    if tomllib.loads(updated_manifest)["package"]["version"] != target:
        raise ValueError("The generated manifest did not contain the target version")
    if root_package(tomllib.loads(updated_lock), name)["version"] != target:
        raise ValueError("The generated lockfile did not contain the target version")
    if not dry_run:
        atomic_update({paths[0]: updated_manifest.encode(), paths[1]: updated_lock.encode()}, originals)
    return target


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("version", nargs="?")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="Check manifest, lockfile and changelog without writing")
    mode.add_argument("--dry-run", action="store_true", help="Validate an update without writing any files")
    args = parser.parse_args()
    try:
        version = update(args.root.resolve(), args.version, check=args.check, dry_run=args.dry_run)
    except (ValueError, KeyError, OSError, RuntimeError, tomllib.TOMLDecodeError) as error:
        parser.exit(1, f"Version check failed: {error}\n")
    action = "Checked" if args.check else ("Would update to" if args.dry_run else "Updated to")
    print(f"{action} {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
