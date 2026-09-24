# Releasing OpenAGC

Releases are signed with Developer ID, notarized and delivered as a DMG. The
app then updates itself with [Sparkle 2](https://sparkle-project.org). See
spec §16. These steps need the Actual AI Apple team and are done by a
maintainer. Nothing here runs in ordinary CI.

## One-time setup

1. **Sparkle signing key.** Build once (`scripts/test-macos.sh build`), then:

   ```bash
   build/DerivedData/SourcePackages/artifacts/sparkle/Sparkle/bin/generate_keys
   ```

   The private key goes into your login Keychain. Back it up
   (`generate_keys -x private.key`) somewhere safe; losing it means users
   can no longer verify updates. Store it as the `SPARKLE_ED_PRIVATE_KEY`
   secret for `release.yml`.
2. **Public key.** Put the printed public key in `macos/project.yml` under
   `SUPublicEDKey`. Until this is set the app's updater is off. Debug
   builds (Xcode runs and tests) never check for updates even with a key.
3. **Apple credentials.** Install the Developer ID Application certificate.
   Create an App Store Connect API key for `notarytool` and add it to the
   CI secrets (see oagc-qtt).

## Each release

1. Set `MARKETING_VERSION` (for example `0.2.0`) and increase
   `CURRENT_PROJECT_VERSION` in `macos/project.yml`. Sparkle compares
   `CURRENT_PROJECT_VERSION`, so it must always go up.
2. Build, sign, notarize and staple the DMG:

   ```bash
   DEVELOPER_ID_APPLICATION="Developer ID Application: Actual AI (TEAMID)" \
   NOTARY_PROFILE=openagc \
   scripts/release.sh 0.2.0            # add --beta for a beta
   ```

   The script builds the Release configuration (Apple Silicon), re-signs
   every nested Mach-O inside out with the hardened runtime and a secure
   timestamp, checks that each one carries the runtime flag (Xcode skips it
   for ad-hoc builds), makes the DMG, notarizes and staples it, and adds it
   to the appcast. Without the two variables it still builds an ad-hoc
   signed DMG — useful to check the pipeline, never to ship. (Ad-hoc dry runs
   switch library validation off so the app can load its own frameworks; a
   Developer ID build keeps it on.)

   `.github/workflows/release.yml` does the same on a `v*` tag. It is not in
   the repository yet: pushing workflow files needs the `workflow` scope
   (`gh auth refresh -h github.com -s workflow`), then `git add -f` it.
3. Optionally write release notes next to the DMG as
   `OpenAGC-0.2.0.md`.
4. Add the release to the appcast:

   ```bash
   scripts/make-appcast.sh build/release/OpenAGC-0.2.0.dmg 0.2.0          # stable
   scripts/make-appcast.sh build/release/OpenAGC-0.3.0-beta1.dmg 0.3.0-beta1 --beta
   ```

5. Create the GitHub release `v0.2.0` (mark betas as pre-releases), upload
   the DMG, and commit the updated `appcast.xml` to `main`. The app reads
   it from `https://raw.githubusercontent.com/audiojak/openagc/main/appcast.xml`.

Beta items carry the Sparkle `beta` channel. Only users who turn on
**Settings › General › Include beta versions** see them.
