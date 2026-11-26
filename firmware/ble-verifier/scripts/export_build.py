#!/usr/bin/env python3
"""Export only actual ESP-IDF flash segments and a source/hash manifest."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys

PROJECT = Path(__file__).resolve().parents[1]
REPOSITORY = PROJECT.parents[1]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def outside_repository(path):
    result = Path(path).resolve()
    if result == REPOSITORY or REPOSITORY in result.parents:
        raise ValueError("Build/export directories must be outside the source repository")
    return result


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--check-dirs":
        outside_repository(sys.argv[2])
        outside_repository(sys.argv[3])
        return
    if len(sys.argv) != 3:
        raise ValueError("Usage: export_build.py BUILD_DIRECTORY EXPORT_DIRECTORY")
    build = outside_repository(sys.argv[1])
    output = outside_repository(sys.argv[2])
    description = json.loads((build / "project_description.json").read_text())
    version = description["project_version"]
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("Build project version is not a stable MAJOR.MINOR.PATCH")
    output.mkdir(parents=True, exist_ok=True)
    args = json.loads((build / "flasher_args.json").read_text())
    if args["extra_esptool_args"]["chip"] != "esp32s3":
        raise ValueError("The actual build is not for ESP32-S3")
    settings = args["flash_settings"]
    expected = {"flash_mode": "dio", "flash_size": "4MB", "flash_freq": "40m"}
    if settings != expected:
        raise ValueError(f"Actual flash settings differ from documented assumptions: {settings}")
    flash_files = {}
    hashes = {}
    pairs = []
    for offset, relative in sorted(args["flash_files"].items(), key=lambda item: int(item[0], 0)):
        source = (build / relative).resolve()
        if build not in source.parents or not source.is_file():
            raise ValueError("Unsafe or missing generated flash segment")
        destination = output / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
        flash_files[offset] = relative
        hashes[relative] = digest(destination)
        pairs.extend([offset, str(destination)])
    merged = output / f"ESP32S3-BLE-Verifier-v{version}-merged.bin"
    command = [sys.executable, "-m", "esptool", "--chip", "esp32s3", "merge_bin", "-o", str(merged)]
    for flag, value in settings.items():
        command.extend([f"--{flag}", value])
    subprocess.run(command + pairs, check=True)
    merged_bytes = merged.read_bytes()
    cursor = 0
    for offset, relative in flash_files.items():
        start = int(offset, 0)
        data = (output / relative).read_bytes()
        if start < cursor or merged_bytes[cursor:start] != b"\xff" * (start - cursor):
            raise ValueError("Unexpected merged overlap or non-FF gap")
        if merged_bytes[start:start + len(data)] != data:
            raise ValueError("Merged segment differs from actual build output")
        cursor = start + len(data)
    if len(merged_bytes) != cursor:
        raise ValueError("Unexpected trailing bytes in merged image")
    for segment in [output / flash_files["0x0"], output / args["app"]["file"]]:
        subprocess.run([sys.executable, "-m", "esptool", "--chip", "esp32s3", "image_info", str(segment)], check=True)
    source_files = {
        str(path.relative_to(REPOSITORY)): digest(path)
        for path in sorted(PROJECT.rglob("*"))
        if path.is_file() and "__pycache__" not in path.parts
    }
    manifest = {"chip": "esp32s3", "version": version, "idf_version": "5.3.2",
                "flash_settings": settings, "flash_files": flash_files, "sha256": hashes,
                "merged": {"file": merged.name, "offset": "0x0", "sha256": digest(merged)},
                "source_files": source_files}
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Verified export: {output / 'manifest.json'}")


if __name__ == "__main__":
    main()
