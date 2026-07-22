# Building and releasing Popsicle for macOS

Popsicle's public macOS artifact is an Apple Silicon disk image containing a
self-contained `Popsicle.app` and an `Applications` shortcut. A public release
must be signed with a **Developer ID Application** certificate, notarized by
Apple, and stapled. Ad-hoc and unsigned images are provided only for local
packaging tests.

## Prerequisites

- Apple Silicon Mac with Xcode Command Line Tools
- Rust toolchain and the packages in `macos/Brewfile`
- Apple Developer Program membership for public releases
- A `Developer ID Application` certificate and its private key in a keychain
- An App Store Connect **Team API key** authorized for notarization, or a
  `notarytool` keychain profile

Build and bundle the application first:

```sh
brew bundle --file macos/Brewfile
macos/build-app.sh
```

Release builds target macOS 14 by default. Homebrew bottles installed on a
newer macOS release may themselves require that release; in that case, use a
matching target for a local-only build, for example
`MACOSX_DEPLOYMENT_TARGET=26.0 macos/build-app.sh`. The release workflow builds
on macOS 14 so its bundled dependencies genuinely support the advertised
minimum version.

The resulting `dist/Popsicle.app` must be self-contained. In particular, every
non-system dynamic library must be inside the bundle and referenced through
`@rpath`, `@loader_path`, or `@executable_path`. Release packaging stops if it
finds an absolute dependency outside `/System/Library` or `/usr/lib`.

## Local packaging tests

Ad-hoc signing exercises nested-code signing, DMG construction, mounting, and
signature verification without contacting Apple:

```sh
RELEASE_TAG=local macos/release-dmg.sh --adhoc --force
```

An entirely unsigned test image can also be produced:

```sh
RELEASE_TAG=local macos/release-dmg.sh --unsigned --force
```

These modes deliberately skip Gatekeeper, notarization, and stapling checks.
They are not suitable for distribution.

## Developer ID release

List the available signing identities:

```sh
security find-identity -v -p codesigning
```

The identity supplied to the release script must resolve to a certificate whose
name starts with `Developer ID Application:`. The script signs every Mach-O
file and nested code bundle explicitly from the inside out. It does not use
`codesign --deep` for signing. Every executable signature enables Hardened
Runtime and uses Apple's secure timestamp service.

### App Store Connect API key

Export an App Store Connect Team API private key to a protected local path and
run:

```sh
export RELEASE_TAG=v1.2.3
export DEVELOPER_ID_APPLICATION='Developer ID Application: Example, Inc. (TEAMID)'
export APP_STORE_CONNECT_API_KEY_PATH="$PWD/private/AuthKey_ABC123XYZ.p8"
export APP_STORE_CONNECT_KEY_ID='ABC123XYZ'
export APP_STORE_CONNECT_ISSUER_ID='00000000-0000-0000-0000-000000000000'

macos/release-dmg.sh
```

Do not commit the `.p8` file, signing certificate, or passwords. An App Store
Connect individual API key is not accepted by `notarytool`; use a Team API key.

### Keychain profile

For an interactive development Mac, credentials may instead be saved in the
login keychain:

```sh
xcrun notarytool store-credentials popsicle-notary \
  --key "$PWD/private/AuthKey_ABC123XYZ.p8" \
  --key-id ABC123XYZ \
  --issuer 00000000-0000-0000-0000-000000000000

RELEASE_TAG=v1.2.3 \
DEVELOPER_ID_APPLICATION='Developer ID Application: Example, Inc. (TEAMID)' \
macos/release-dmg.sh --keychain-profile popsicle-notary
```

Use `--notary-keychain PATH` when the stored profile is in a non-default
keychain. Use `--signing-keychain PATH` when the Developer ID identity is in a
non-default keychain. Run `macos/release-dmg.sh --help` for all CLI and
environment options.

## What the release script does

1. Copies the app to a private staging directory and removes extended
   attributes from the copy.
2. Rejects non-system absolute dynamic-library references.
3. Signs Mach-O files, nested bundles, and finally the outer app with Hardened
   Runtime and a secure timestamp.
4. Runs `codesign` and, when available, `syspolicy_check notary-submission`.
5. Submits a ZIP of the app to `notarytool`, prints the complete notarization
   log, and staples and validates the app ticket.
6. Creates a temporary APFS image with a clean volume name and `/Applications`
   symlink, then converts it to a zlib-compressed, read-only DMG.
7. Signs and separately notarizes the DMG, prints its notarization log, and
   staples and validates its ticket.
8. Runs `hdiutil`, `codesign`, `spctl`, `stapler`, and `syspolicy_check`
   verification, mounts the image read-only, and verifies its contents.

The app is notarized before it is copied into the DMG so its ticket remains
stapled after a user copies it to `/Applications`. The DMG is then notarized
separately so the downloaded container also carries its own ticket.

The default artifact name is:

```text
dist/Popsicle-${RELEASE_TAG}-arm64.dmg
```

The output is written only after all checks pass. Existing output is preserved
unless `--force` is supplied. The original `dist/Popsicle.app` is never
modified.

For GitHub Actions, place signing and notarization credentials in a protected
`macos-release` environment. The workflow uses these exact secret names:

- `DEVELOPER_ID_P12_BASE64`
- `DEVELOPER_ID_P12_PASSWORD`
- `APP_STORE_CONNECT_API_KEY_P8_BASE64`
- `APP_STORE_CONNECT_KEY_ID`
- `APP_STORE_CONNECT_ISSUER_ID`

It decodes the API key to a temporary file and invokes the script with this
environment interface:

- `RELEASE_TAG`
- `DEVELOPER_ID_APPLICATION`
- `APP_STORE_CONNECT_API_KEY_PATH`
- `APP_STORE_CONNECT_KEY_ID`
- `APP_STORE_CONNECT_ISSUER_ID`

Import the Developer ID certificate into a temporary keychain for the signing
job, add that keychain to the search list, and delete it when the job finishes.
Configure the `macos-release` environment to allow only protected `v*` tags and
require a reviewer. A manual `workflow_dispatch` release must be launched with
an existing `vX.Y.Z` tag selected; the workflow intentionally rejects branch
references. A repository tag ruleset and GitHub immutable releases are also
recommended so a published tag and its assets cannot be silently replaced.

## Manual verification

The script performs these checks automatically. They are useful again after
downloading the asset from GitHub, because the downloaded copy carries the
quarantine attribute that triggers Gatekeeper:

```sh
hdiutil verify dist/Popsicle-v1.2.3-arm64.dmg
codesign --verify --verbose=4 dist/Popsicle-v1.2.3-arm64.dmg
xcrun stapler validate -v dist/Popsicle-v1.2.3-arm64.dmg
spctl --assess --type open --context context:primary-signature --verbose=4 \
  dist/Popsicle-v1.2.3-arm64.dmg

hdiutil attach -readonly -nobrowse dist/Popsicle-v1.2.3-arm64.dmg
codesign --verify --deep --strict --verbose=4 /Volumes/'Popsicle v1.2.3'/Popsicle.app
spctl --assess --type execute --verbose=4 /Volumes/'Popsicle v1.2.3'/Popsicle.app
syspolicy_check distribution /Volumes/'Popsicle v1.2.3'/Popsicle.app --verbose
```

Test the downloaded DMG on a clean Apple Silicon Mac without Homebrew. Also
test once while offline; successful offline launch confirms that the stapled
tickets are usable without contacting Apple's notary service.
