#!/usr/bin/env bash
# Builds Ferrotime.app around a compiled binary and packages it as a .dmg.
#
#   packaging/macos/package.sh <binary> <version> <output-dir>
#
# Optional, through the environment:
#   SIGN_IDENTITY       codesign identity, e.g. "Developer ID Application: Name (TEAMID)".
#                       Unset: the app and disk image are left unsigned.
#   NOTARY_KEY_PATH     App Store Connect API key (.p8), with NOTARY_KEY_ID and NOTARY_ISSUER_ID,
#   NOTARY_PROFILE      or a keychain profile created with `xcrun notarytool store-credentials`.
#                       Either one (with SIGN_IDENTITY) notarizes and staples both the app and the
#                       disk image, so Gatekeeper accepts them without a network connection.
#   SIGN_KEYCHAIN       keychain holding SIGN_IDENTITY, if not in the default search list.
set -euo pipefail

if [ $# -ne 3 ]; then
	echo "usage: $0 <binary> <version> <output-dir>" >&2
	exit 2
fi
binary=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app="$work/Ferrotime.app"

mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"
cp "$binary" "$app/Contents/MacOS/ferrotime"
chmod 755 "$app/Contents/MacOS/ferrotime"
sed "s/@VERSION@/$version/g" "$here/Info.plist" >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

sign() {
	local args=(--force --timestamp --sign "$SIGN_IDENTITY")
	if [ -n "${SIGN_KEYCHAIN:-}" ]; then
		args+=(--keychain "$SIGN_KEYCHAIN")
	fi
	codesign "${args[@]}" "$@"
}

notarize() {
	if [ -n "${NOTARY_KEY_PATH:-}" ]; then
		xcrun notarytool submit "$1" --wait --timeout 30m \
			--key "$NOTARY_KEY_PATH" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID"
	else
		xcrun notarytool submit "$1" --wait --timeout 30m --keychain-profile "$NOTARY_PROFILE"
	fi
}

notarizing=false
if [ -n "${SIGN_IDENTITY:-}" ] && { [ -n "${NOTARY_KEY_PATH:-}" ] || [ -n "${NOTARY_PROFILE:-}" ]; }; then
	notarizing=true
fi

if [ -n "${SIGN_IDENTITY:-}" ]; then
	echo "Signing Ferrotime.app"
	sign --options runtime "$app"
	codesign --verify --strict --verbose=2 "$app"
	if $notarizing; then
		echo "Notarizing Ferrotime.app"
		ditto -c -k --keepParent "$app" "$work/Ferrotime.zip"
		notarize "$work/Ferrotime.zip"
		xcrun stapler staple "$app"
	fi
fi

dmg="$out/ferrotime-$version-macos-universal.dmg"
staging="$work/dmg"
mkdir "$staging"
ditto "$app" "$staging/Ferrotime.app"
ln -s /Applications "$staging/Applications"
cp "$here/../../LICENSE" "$staging/LICENSE.txt"
rm -f "$dmg"
hdiutil create -quiet -volname "Ferrotime $version" -srcfolder "$staging" -fs HFS+ -format UDZO -ov "$dmg"

if [ -n "${SIGN_IDENTITY:-}" ]; then
	echo "Signing the disk image"
	sign "$dmg"
	if $notarizing; then
		echo "Notarizing the disk image"
		notarize "$dmg"
		xcrun stapler staple "$dmg"
		spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
	fi
fi

echo "$dmg"
