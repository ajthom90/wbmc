#!/usr/bin/env bash
# Build the Rust core for macOS (arm64 + x86_64), generate the Swift bindings, and
# package the universal static library as WBMCCoreFFI.xcframework for WBMCCoreKit.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

export MACOSX_DEPLOYMENT_TARGET=26.0

targets=(aarch64-apple-darwin x86_64-apple-darwin)
lib_name=libwbmc_ffi.a
kit="$root/apps/apple/WBMCCoreKit"
generated="$kit/Sources/WBMCCore/Generated"
xcframework="$kit/WBMCCoreFFI.xcframework"

installed="$(rustup target list --installed)"
for target in "${targets[@]}"; do
    if ! grep -qx "$target" <<<"$installed"; then
        rustup target add "$target"
    fi
done

for target in "${targets[@]}"; do
    cargo build -p wbmc-ffi --features debug-tools --release --target "$target"
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

universal="$work/$lib_name"
lipo -create \
    "target/aarch64-apple-darwin/release/$lib_name" \
    "target/x86_64-apple-darwin/release/$lib_name" \
    -output "$universal"

bindings="$work/bindings"
cargo run -p wbmc-ffi --features uniffi-cli --bin uniffi-bindgen -- generate \
    --library "target/aarch64-apple-darwin/release/$lib_name" \
    --language swift \
    --out-dir "$bindings"

rm -rf "$generated"
mkdir -p "$generated"
cp "$bindings"/*.swift "$generated/"

headers="$work/headers"
mkdir -p "$headers"
cp "$bindings"/*.h "$headers/"
modulemaps=("$bindings"/*.modulemap)
if [[ ${#modulemaps[@]} -ne 1 ]]; then
    echo "expected exactly one generated modulemap, found ${#modulemaps[@]}" >&2
    exit 1
fi
cp "${modulemaps[0]}" "$headers/module.modulemap"

rm -rf "$xcframework"
xcodebuild -create-xcframework \
    -library "$universal" \
    -headers "$headers" \
    -output "$xcframework"

echo "Built $xcframework"
echo "Generated $(ls "$generated")"
