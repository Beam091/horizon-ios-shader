#!/usr/bin/env bash
# Packages the game for iOS: Horizon.app (the binary, Info.plist, the icon) in an unsigned
# Horizon.ipa. A sideloading tool (Sideloadly, AltStore) signs it with the player's own Apple ID
# when it installs it; with IOS_SIGN_IDENTITY set (an "Apple Development: …" identity) and
# IOS_PROVISIONING_PROFILE naming a .mobileprovision, it is signed here instead.
#
#   packaging/ios/package.sh BINARY VERSION OUTDIR
#
# IOS_INSTANCE (digits) makes it a separate instance: identifier dev.horizon.game.i<digits>, link
# horizon-i<digits>://, so it installs beside Horizon and other instances; IOS_NAME names it on the
# home screen.
#
# Runs on macOS (sips, plutil, ditto, lipo) and on Linux (python3 + Pillow, zip, llvm-objdump). Horizon ships none of Mojang's files: the player copies the
# prepared resources into the app's Documents folder with the Files app (packaging/ios/README.md).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
binary=$1
version=$2
out=$(mkdir -p "$3" && cd "$3" && pwd)
if command -v lipo >/dev/null; then
  [[ "$(lipo -archs "$binary")" == "arm64" ]] || { echo "$binary is not an arm64 binary" >&2; exit 1; }
else
  # Linux: the Mach-O header says cputype ARM64 (0x0100000c, little-endian "0c 00 00 01").
  [[ "$(head -c 8 "$binary" | od -An -tx1 | tr -d ' \n')" == "cffaedfe0c000001" ]] \
    || { echo "$binary is not an arm64 Mach-O binary" >&2; exit 1; }
fi

stage=$(mktemp -d)
app=$stage/Payload/Horizon.app
mkdir -p "$app"
cp "$binary" "$app/horizon-client"
chmod 755 "$app/horizon-client"
instance=${IOS_INSTANCE:-}
[[ -z "$instance" || "$instance" =~ ^[0-9]{4,12}$ ]] || { echo "IOS_INSTANCE must be 4-12 digits" >&2; exit 1; }
if [[ -n "$instance" ]]; then
  bundle=dev.horizon.game.i$instance scheme=horizon-i$instance name=${IOS_NAME:-Horizon $instance}
else
  bundle=dev.horizon.game scheme=horizon name=${IOS_NAME:-Horizon}
fi
# The home screen name, as plain text in the plist.
name=$(printf '%s' "$name" | tr -d '<>&"\\/' | cut -c1-30)
sed -e "s/@VERSION@/$version/g" -e "s/@BUNDLE@/$bundle/g" -e "s/@SCHEME@/$scheme/g" -e "s/@NAME@/$name/g" \
  "$here/Info.plist" > "$app/Info.plist"
if command -v xcrun >/dev/null && command -v plutil >/dev/null; then
  sdk_ver=$(xcrun --sdk iphoneos --show-sdk-version 2>/dev/null || true)
  sdk_build=$(xcrun --sdk iphoneos --show-sdk-build-version 2>/dev/null || true)
  macos_build=$(sw_vers -buildVersion 2>/dev/null || true)
  xcode_ver_line=$(xcodebuild -version 2>/dev/null || true)
  xcode_ver=$(printf '%s\n' "$xcode_ver_line" | sed -n 's/^Xcode \([0-9]*\)\.\([0-9]*\).*/\1\2/p')
  xcode_build=$(printf '%s\n' "$xcode_ver_line" | sed -n 's/^Build version \(.*\)/\1/p')
  plutil -replace DTPlatformName -string iphoneos "$app/Info.plist"
  plutil -replace DTCompiler -string com.apple.compilers.llvm.clang.1_0 "$app/Info.plist"
  if [[ -n "$sdk_ver" ]]; then
    plutil -replace DTPlatformVersion -string "$sdk_ver" "$app/Info.plist"
    plutil -replace DTSDKName -string "iphoneos$sdk_ver" "$app/Info.plist"
  fi
  if [[ -n "$sdk_build" ]]; then
    plutil -replace DTPlatformBuild -string "$sdk_build" "$app/Info.plist"
    plutil -replace DTSDKBuild -string "$sdk_build" "$app/Info.plist"
  fi
  if [[ -n "$xcode_ver" ]]; then
    # Format e.g. 27.2 -> 2720 (4-digit DTXcode convention).
    dt_xcode=$(printf '%04d' "$((10#$xcode_ver * 10))")
    plutil -replace DTXcode -string "$dt_xcode" "$app/Info.plist"
  fi
  if [[ -n "$xcode_build" ]]; then
    plutil -replace DTXcodeBuild -string "$xcode_build" "$app/Info.plist"
  fi
  if [[ -n "$macos_build" ]]; then
    plutil -replace BuildMachineOSBuild -string "$macos_build" "$app/Info.plist"
  fi
fi
if command -v plutil >/dev/null; then plutil -lint "$app/Info.plist"; else python3 -c 'import plistlib,sys; plistlib.load(open(sys.argv[1],"rb"))' "$app/Info.plist"; fi
printf 'APPL????' > "$app/PkgInfo"
icon=$here/icon.png
resize() { # SIZE OUT
  if command -v sips >/dev/null; then sips -z "$1" "$1" "$icon" --out "$2" >/dev/null
  else python3 -c 'import sys; from PIL import Image; Image.open(sys.argv[1]).convert("RGB").resize((int(sys.argv[2]),)*2, Image.LANCZOS).save(sys.argv[3])' "$icon" "$1" "$2"; fi
}
resize 120 "$app/AppIcon60x60@2x.png"
resize 180 "$app/AppIcon60x60@3x.png"

if [[ -n "${IOS_SIGN_IDENTITY:-}" ]]; then
  cp "$IOS_PROVISIONING_PROFILE" "$app/embedded.mobileprovision"
  codesign --force --sign "$IOS_SIGN_IDENTITY" --timestamp=none "$app"
fi

rm -f "$out/Horizon.ipa"
if command -v ditto >/dev/null; then
  (cd "$stage" && ditto -c -k --sequesterRsrc --keepParent Payload "$out/Horizon.ipa")
else
  (cd "$stage" && zip -qr -X "$out/Horizon.ipa" Payload)
fi
rm -rf "$stage"
echo "$out/Horizon.ipa"
