#!/usr/bin/env python3
"""Offline firmware packaging only; this program never invokes GitHub or writes Git.

The build owner supplies flash_files, sha256 and source_files hash maps in the
export manifest. --check may inspect the worktree; --write requires a commit
whose explicit source blobs exactly match that build snapshot.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tempfile
import zipfile

TAG_PREFIX = "esp32s3-verifier-v"
SOURCE_ROOT = PurePosixPath("firmware/ble-verifier")
DEFAULT_OUTPUT = Path(os.environ.get("RUNNER_TEMP", "/workspace/shared")).resolve()
MAX_BINARY = 64 * 1024 * 1024
PUBLIC_SOURCE_NAMES = {
    "CMakeLists.txt", "sdkconfig.defaults", "partitions.csv", "README.md", ".gitignore",
    "LICENSE", "LICENSE.md", "NOTICE", "THIRD_PARTY_NOTICES.md",
    "main/CMakeLists.txt", "main/app_main.c", "main/ble_protocol.c", "main/ble_protocol.h",
    "tests/test_protocol.c", "scripts/build.sh", "scripts/export_build.py",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha(data):
    return hashlib.sha256(data).hexdigest()


def relative(value):
    require(isinstance(value, str) and value, "Empty or invalid manifest path")
    path = PurePosixPath(value)
    require(not path.is_absolute() and str(path) == value, "Manifest paths must be canonical relative paths")
    require(not any(part in ("", ".", "..") or ":" in part or "\\" in part for part in path.parts), "Unsafe manifest path")
    require(not any(ord(c) < 32 or ord(c) == 127 or c in "\u0085\u2028\u2029" for c in value), "Control characters are not allowed in package paths")
    reserved = {"CON", "PRN", "AUX", "NUL"} | {f"{prefix}{n}" for prefix in ("COM", "LPT") for n in range(1, 10)}
    require(not any(part.endswith((" ", ".")) or part.split(".", 1)[0].upper() in reserved for part in path.parts), "Package path is not portable to Windows")
    return path


def binary(root, name, expected):
    path = root.joinpath(*relative(name).parts)
    require(path.resolve().is_relative_to(root), "Binary escapes the export directory")
    require(path.is_file() and not path.is_symlink(), "Manifest binary is missing or is a symlink")
    require(0 < path.stat().st_size <= MAX_BINARY, "Manifest binary has an invalid size")
    require(isinstance(expected, str) and re.fullmatch(r"[0-9a-fA-F]{64}", expected), "Missing build-time binary SHA256")
    data = path.read_bytes()
    require(sha(data) == expected.lower(), "A binary differs from its build-time SHA256")
    return data


def offset(value):
    require(isinstance(value, (str, int)) and not isinstance(value, bool), "Invalid flash offset")
    result = int(value, 0) if isinstance(value, str) else value
    require(0 <= result <= 0xFFFFFFFF, "Flash offset is outside uint32")
    return result


def git(repo, *arguments):
    result = subprocess.run(["git", *arguments], cwd=repo, capture_output=True, timeout=30)
    require(result.returncode == 0, "Read-only Git lookup failed; commit must contain the public firmware source")
    return result.stdout


def public_source(path):
    path = relative(path)
    require(path.is_relative_to(SOURCE_ROOT), "Source path is outside firmware/ble-verifier")
    require(str(path.relative_to(SOURCE_ROOT)) in PUBLIC_SOURCE_NAMES, "Source file is not in the reviewed public release allowlist")
    forbidden = {".git", "build", ".pio", "dist", "attachments", "logs", "records", "managed_components"}
    require(not any(part.lower() in forbidden for part in path.parts), "Generated/private source path is not allowed")
    require(not path.name.lower().startswith(".env") and path.name.lower() not in {"credentials", "secrets.json"}, "Credential configuration is not a public source input")
    require(path.suffix.lower() not in {".bin", ".elf", ".map", ".zip", ".exe", ".lnk", ".ino", ".log", ".key", ".pem", ".pfx", ".p12"}, "Non-source/private file type is not allowed")
    return path


def snapshot(repo, declared, commit):
    require(isinstance(declared, dict) and 0 < len(declared) <= 256, "source_files must explicitly list the public source hashes")
    sources = {}
    modes = {}
    if commit:
        for entry in git(repo, "ls-tree", "-r", "-z", commit, "--", str(SOURCE_ROOT)).split(b"\0"):
            if not entry:
                continue
            attributes, name = entry.split(b"\t", 1)
            mode, kind, _ = attributes.split()
            require(kind == b"blob" and mode in (b"100644", b"100755"), "Public firmware source contains a symlink/submodule")
            modes[name.decode("utf-8")] = mode
        require(set(modes) == set(declared), "Commit firmware files differ from the explicit build source snapshot")
    for name, expected in sorted(declared.items()):
        path = public_source(name)
        require(isinstance(expected, str) and re.fullmatch(r"[0-9a-fA-F]{64}", expected), "Invalid source SHA256")
        if commit:
            data = git(repo, "show", f"{commit}:{path}")
        else:
            local = repo.joinpath(*path.parts)
            require(local.resolve().is_relative_to(repo) and local.is_file() and not local.is_symlink(), "Missing/unsafe worktree source")
            data = local.read_bytes()
        require(len(data) <= 4 * 1024 * 1024 and sha(data) == expected.lower(), "Source no longer matches the verified build snapshot")
        sources[str(path.relative_to(SOURCE_ROOT))] = data
    require("CMakeLists.txt" in sources and any(name.startswith("main/") and name.endswith(".c") for name in sources), "Public firmware build inputs are incomplete")
    return sources


def json_bytes(value):
    return (json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sums(files):
    return "".join(f"{sha(data)}  {name}\n" for name, data in sorted(files.items())).encode("utf-8")


def archive(files):
    buffer = io.BytesIO()
    require(len({name.casefold() for name in files}) == len(files), "Package paths collide on Windows")
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as output:
        for name, data in sorted(files.items()):
            relative(name)
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            output.writestr(info, data)
    data = buffer.getvalue()
    with zipfile.ZipFile(io.BytesIO(data)) as check:
        require(check.testzip() is None, "Generated ZIP CRC verification failed")
        require(set(check.namelist()) == set(files), "Generated ZIP member set mismatch")
        for name, expected in files.items():
            require(check.read(name) == expected, "Generated ZIP member verification failed")
    return data


def write_new(path, data):
    if path.exists():
        require(path.is_file() and not path.is_symlink() and path.read_bytes() == data, "An existing release output has different bytes; nothing is overwritten")
        return
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix=".firmware-package-", delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    try:
        os.link(temporary, path)  # Atomic creation, never replace an existing artifact.
    except FileExistsError:
        require(path.is_file() and not path.is_symlink() and path.read_bytes() == data, "Concurrent output differs; not overwritten")
    finally:
        temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path("/workspace/hw_hub"))
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--commit", help="Final committed public source; required for --write")
    parser.add_argument("--tag", help="Expected dedicated firmware tag; must match the manifest version")
    parser.add_argument("--output-dir", type=Path)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true", help="Offline preflight; no output files are written")
    mode.add_argument("--write", action="store_true", help="Create immutable local artifacts, never publish")
    args = parser.parse_args()
    repo = args.repo.resolve()
    manifest_path = args.manifest.resolve()
    require(manifest_path.is_file() and manifest_path.stat().st_size <= 1024 * 1024, "Missing/oversized build export manifest")
    build_root = manifest_path.parent
    manifest = json.loads(manifest_path.read_text(encoding="utf-8-sig"))
    require(manifest.get("chip", "").lower().replace("-", "") == "esp32s3", "Build target is not ESP32-S3")
    version = manifest.get("version")
    require(isinstance(version, str) and re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version), "Firmware version must be stable MAJOR.MINOR.PATCH")
    tag = TAG_PREFIX + version
    stem = "ESP32-S3-Verifier-v" + version
    require(args.tag is None or args.tag == tag, "Firmware tag differs from the build version")
    idf = manifest.get("idf_version", manifest.get("idf"))
    require(isinstance(idf, str) and idf.removeprefix("v") == "5.3.2", "Build must identify the pinned ESP-IDF 5.3.2")
    settings = manifest.get("flash_settings")
    require(isinstance(settings, dict) and settings, "Missing actual build flash_settings")
    require(set(settings).issubset({"flash_mode", "flash_size", "flash_freq"}) and all(isinstance(value, str) and not any(c in value for c in "\r\n\0") for value in settings.values()), "Unexpected flash_settings fields")
    flash_files = manifest.get("flash_files")
    hashes = manifest.get("sha256")
    require(isinstance(flash_files, dict) and 0 < len(flash_files) <= 16 and isinstance(hashes, dict), "Missing actual flash_files or binary hashes")
    images = {}
    segments = []
    checked_images = 0
    for address, name in flash_files.items():
        start = offset(address)
        data = binary(build_root, name, hashes.get(name))
        require(start + len(data) <= 0x100000000, "A flash image exceeds uint32 addressing")
        if data[0] == 0xE9:
            require(len(data) >= 24 and int.from_bytes(data[12:14], "little") == 9, "Espressif image header is not ESP32-S3")
            checked_images += 1
        require(name not in images, "Duplicate flash file")
        images[name] = data
        segments.append((start, name, data))
    require(checked_images > 0, "No ESP32-S3 executable image was found")
    segments.sort()
    require(all(left[0] + len(left[2]) <= right[0] for left, right in zip(segments, segments[1:])), "Flash segments overlap")
    merged = manifest.get("merged")
    require(isinstance(merged, dict), "Missing actual esptool merged firmware")
    merged_name = merged.get("file")
    merged_data = binary(build_root, merged_name, merged.get("sha256", hashes.get(merged_name)))
    merged_start = offset(merged.get("offset"))
    expected = bytearray(b"\xff") * len(merged_data)
    for start, _, data in segments:
        position = start - merged_start
        require(position >= 0 and position + len(data) <= len(expected), "Merged firmware does not cover flash segments")
        expected[position:position + len(data)] = data
    require(bytes(expected) == merged_data, "Merged firmware differs from declared images/FF padding")
    require(merged_name not in images, "Merged firmware must have a distinct filename")
    images[merged_name] = merged_data
    commit = None
    if args.commit:
        commit = git(repo, "rev-parse", "--verify", "--end-of-options", args.commit + "^{commit}").decode("ascii").strip()
        require(re.fullmatch(r"[0-9a-f]{40,64}", commit), "Invalid resolved commit")
    require(not args.write or commit is not None, "--write requires --commit; uncommitted source is preflight-only")
    sources = snapshot(repo, manifest.get("source_files"), commit)
    require(re.search(r'\bset\s*\(\s*PROJECT_VER\s+"' + re.escape(version) + r'"\s*\)', sources["CMakeLists.txt"].decode("utf-8")), "Tagged CMake project version differs from the build manifest")
    metadata = {
        "product": "ESP32-S3 Verifier", "firmware_version": version, "release_tag": tag,
        "target": "esp32s3", "idf_version": idf, "source_commit": commit,
        "source_files": manifest["source_files"], "sha256": {name: sha(data) for name, data in sorted(images.items())},
    }
    packaged_manifest = {
        "chip": "esp32s3", "version": version, "flash_settings": settings,
        "flash_files": {f"0x{start:x}": "images/" + name for start, name, _ in segments},
        "merged": {"file": "images/" + merged_name, "offset": f"0x{merged_start:x}", "sha256": sha(merged_data)},
    }
    readme = (
        f"ESP32-S3 Verifier {version}\n\n"
        "This package contains newly built firmware, not a device flash dump.\n"
        "Choose ESP32-S3 in FlashTool. Use the actual offsets in flash-manifest.json.\n"
        f"Merged image: images/{merged_name}, offset 0x{merged_start:x}.\n"
        "Check SHA256SUMS before flashing. SDK/partition/settings and hardware must match.\n"
        "Building and packaging do not constitute real-board validation.\n\n"
        "Public project source is included under source/.\n"
    ).encode("utf-8")
    firmware = {"images/" + name: data for name, data in images.items()}
    firmware.update({"source/" + name: data for name, data in sources.items()})
    firmware.update({"flash-manifest.json": json_bytes(packaged_manifest), "BUILD-METADATA.json": json_bytes(metadata), "README-Flash.txt": readme})
    firmware["SHA256SUMS"] = sums(firmware)
    source = {"source/" + name: data for name, data in sources.items()}
    source["BUILD-METADATA.json"] = json_bytes(metadata)
    source["SHA256SUMS"] = sums(source)
    outputs = {stem + "-firmware.zip": archive(firmware), stem + "-source.zip": archive(source)}
    outputs["SHA256SUMS"] = sums(outputs)
    plan = {
        "tag": tag, "make_latest": False, "source_commit": commit,
        "publication_ready": commit is not None, "local_mode": "write" if args.write else "check-only",
        "binary_count": len(images), "source_file_count": len(sources),
        "assets": {name: {"bytes": len(data), "sha256": sha(data)} for name, data in sorted(outputs.items())},
    }
    print(json.dumps(plan, ensure_ascii=False, indent=2))
    if args.write:
        destination = (args.output_dir or DEFAULT_OUTPUT / tag).resolve()
        require(not destination.is_relative_to(repo), "Build output must stay outside the source repository")
        destination.mkdir(parents=True, exist_ok=True)
        outputs["release-plan.json"] = json_bytes(plan)
        for name, data in outputs.items():
            path = destination / name
            require(not path.exists() or path.is_file() and not path.is_symlink() and path.read_bytes() == data, "Existing local output differs; nothing overwritten")
        for name, data in outputs.items():
            write_new(destination / name, data)
        descriptor = os.open(destination, os.O_RDONLY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
        print("Local artifacts verified: " + str(destination))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError, subprocess.TimeoutExpired, zipfile.BadZipFile) as error:
        print("Firmware packaging failed: " + str(error), file=__import__("sys").stderr)
        raise SystemExit(1)
