#!/usr/bin/env bash
set -euo pipefail
project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
build_dir=${1:-"${TMPDIR:-/tmp}/ble-verifier-build"}
export_dir=${2:-"${TMPDIR:-/tmp}/ble-verifier-export"}
if [[ -z ${IDF_PATH:-} ]] || ! command -v idf.py >/dev/null; then
    echo 'Activate ESP-IDF v5.3.2 before running this script.' >&2
    exit 1
fi
idf_version=$(idf.py --version)
[[ $idf_version =~ v5\.3\.2($|[^0-9]) ]] || { echo 'ESP-IDF v5.3.2 is required.' >&2; exit 1; }
python "$project_dir/scripts/export_build.py" --check-dirs "$build_dir" "$export_dir"
mkdir -p "$build_dir"
defaults_hash=$(python -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest()[:16])' "$project_dir/sdkconfig.defaults")
idf.py -C "$project_dir" -B "$build_dir" -D IDF_TARGET=esp32s3 \
    -D "SDKCONFIG=$build_dir/sdkconfig-$defaults_hash" build
cc -std=c11 -Wall -Wextra -Werror -I"$project_dir/main" \
    "$project_dir/main/ble_protocol.c" "$project_dir/tests/test_protocol.c" -o "$build_dir/protocol-tests"
"$build_dir/protocol-tests"
python "$project_dir/scripts/export_build.py" "$build_dir" "$export_dir"
