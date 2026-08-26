#!/bin/bash
# Phase-0 de-risk: build a host .app containing a hello-world FSKit .appex, ad-hoc
# signed with the FSKit entitlement, and stage it in /Applications so fskitd can
# register it. This is NOT the production build (that's an Xcode/Tauri concern — see
# BUILD.md); it's the minimal path to answer "can a third-party FSKit module mount on
# THIS box with the signing we have?".
set -euo pipefail

export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="$(xcrun --sdk macosx --show-sdk-path)"
OUT="${OUT:-/tmp/hellofs-build}"
APP="$OUT/MakerHelloFS.app"
APPEX="$APP/Contents/PlugIns/FskitHello.appex"

rm -rf "$OUT"
mkdir -p "$APPEX/Contents/MacOS" "$APP/Contents/MacOS"

echo "==> compiling the FSKit extension binary"
# Note: the @main entry lives in HelloFSExtension.swift (a UnaryFileSystemExtension); we do
# NOT compile a separate main.swift (that would collide with the synthesized @main `main`).
xcrun swiftc -sdk "$SDK" -target arm64-apple-macos15.4 -framework FSKit -framework ExtensionFoundation \
  -parse-as-library \
  -o "$APPEX/Contents/MacOS/FskitHello" \
  "$HERE/HelloFS.swift" "$HERE/HelloFSExtension.swift"

echo "==> compiling a trivial host app (just needs to exist to carry the appex)"
cat > "$OUT/hostmain.swift" <<'SWIFT'
import Foundation
print("MakerHelloFS host — the FSKit module lives in PlugIns/. Enable it in System Settings > General > Login Items & Extensions > File System Extensions.")
SWIFT
xcrun swiftc -sdk "$SDK" -target arm64-apple-macos15.4 -o "$APP/Contents/MacOS/MakerHelloFS" "$OUT/hostmain.swift"

echo "==> writing Info.plists"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>MakerHelloFS</string>
  <key>CFBundleIdentifier</key><string>com.maker.fskit.hello.host</string>
  <key>CFBundleName</key><string>MakerHelloFS</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>15.4</string>
</dict></plist>
PLIST

# The appex Info.plist mirrors the SHIPPING FUSE-T module's plist shape as closely as
# possible (reverse-engineered from /Applications/fuse-t.app/.../FskitSrvModule.appex):
# XPC! package, the fsmodule extension point, the full EXAppExtensionAttributes key set
# (FSMediaTypes/FSPersonalities/FSRequiresSecurityScopedPathURLResources), plus the
# CFBundleSupportedPlatforms + DT* SDK-version keys + LSMinimumSystemVersion=26.0 that a
# real Xcode build stamps. (Hypothesis 1, 2026-06-14: ExtensionFoundation on macOS 26 may
# decline to keep main() resident if the appex doesn't declare a modern-SDK macOS-26 build.)
SDKVER="$(xcrun --sdk macosx --show-sdk-version)"            # e.g. 26.5
cat > "$APPEX/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>FskitHello</string>
  <key>CFBundleExecutable</key><string>FskitHello</string>
  <key>CFBundleIdentifier</key><string>com.maker.fskit.hello.module</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>FskitHello</string>
  <key>CFBundlePackageType</key><string>XPC!</string>
  <key>CFBundleShortVersionString</key><string>0.1</string>
  <key>CFBundleSupportedPlatforms</key><array><string>MacOSX</string></array>
  <key>CFBundleVersion</key><string>1</string>
  <key>DTCompiler</key><string>com.apple.compilers.llvm.clang.1_0</string>
  <key>DTPlatformName</key><string>macosx</string>
  <key>DTPlatformVersion</key><string>$SDKVER</string>
  <key>DTSDKName</key><string>macosx$SDKVER</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>EXAppExtensionAttributes</key><dict>
    <key>EXExtensionPointIdentifier</key><string>com.apple.fskit.fsmodule</string>
    <key>FSActivateOptionSyntax</key><dict><key>shortOptions</key><string>o:</string></dict>
    <key>FSMediaTypes</key><dict/>
    <key>FSPersonalities</key><dict/>
    <key>FSRequiresSecurityScopedPathURLResources</key><true/>
    <key>FSShortName</key><string>makerhello</string>
    <key>FSSupportsBlockResources</key><false/>
    <key>FSSupportsPathURLs</key><true/>
  </dict>
</dict></plist>
PLIST

echo "==> ad-hoc signing (appex first, then app) with the FSKit entitlement"
cat > "$OUT/module.entitlements" <<ENT
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.developer.fskit.fsmodule</key><true/>
  <key>com.apple.security.app-sandbox</key><true/>
</dict></plist>
ENT
codesign -f -s - --timestamp=none --generate-entitlement-der --entitlements "$OUT/module.entitlements" "$APPEX"
codesign -f -s - --timestamp=none --generate-entitlement-der "$APP"

echo "==> done: $APP"
echo "    appex: $APPEX"
