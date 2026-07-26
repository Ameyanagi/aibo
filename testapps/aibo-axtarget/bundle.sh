#!/bin/sh
# Wrap the binary in a minimal .app bundle.
#
# Why bother: the S2 matrix is keyed on **bundle identifier** (§8 — the
# AX-tree-enabling flag is chosen by app identity, Chrome wanting
# `AXEnhancedUserInterface` and Electron wanting `AXManualAccessibility`). A bare
# `cargo run` binary has no bundle id, so the control row in the matrix would be
# the one row with a blank key. It also gives the target a stable name in the
# Dock and in `NSWorkspace.frontmostApplication`.
#
# This is NOT signed and NOT notarised. It is a test fixture; §19's signing chain
# is S8's problem.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
profile=${1:-debug}
case "$profile" in
  debug)   cargo build --manifest-path "$here/Cargo.toml" ;;
  release) cargo build --release --manifest-path "$here/Cargo.toml" ;;
  *) echo "usage: $0 [debug|release]" >&2; exit 2 ;;
esac

binary="$here/target/$profile/aibo-axtarget"
app="$here/target/$profile/aibo-axtarget.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp "$binary" "$app/Contents/MacOS/aibo-axtarget"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>              <string>aibo AX target</string>
  <key>CFBundleDisplayName</key>       <string>aibo AX target</string>
  <key>CFBundleExecutable</key>        <string>aibo-axtarget</string>
  <key>CFBundleIdentifier</key>        <string>com.aibo.axtarget</string>
  <key>CFBundlePackageType</key>       <string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key>           <string>1</string>
  <key>LSMinimumSystemVersion</key>    <string>13.0</string>
  <key>NSHighResolutionCapable</key>   <true/>
</dict>
</plist>
PLIST

echo "built $app"
echo "bundle id: com.aibo.axtarget   <- use this as the matrix key"
echo
echo "run it with:  open '$app'"
echo "stdout (the count oracle) goes to Console.app when launched via 'open';"
echo "to see it in a terminal, run the binary directly instead:"
echo "  '$app/Contents/MacOS/aibo-axtarget'"
