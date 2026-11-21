# Signing and notarization procedure

This procedure matches the existing packaging scripts. It does not provide or
authorize any credential. No signing identity is present in the repository or
CI.

## Current artifact state

- Windows executables and the Inno Setup installer are **unsigned** unless
  signing environment variables are supplied.
- macOS bundles receive an **ad-hoc signature** unless `CODESIGN_IDENTITY` is
  supplied. Ad-hoc builds are **not notarized** and Gatekeeper assessment does
  not pass.
- Linux packages are not signed at the package-manager repository level.

## Windows Authenticode

### Required material

- An Authenticode code-signing certificate trusted for the Flux publisher
  identity, preferably stored in the Windows certificate store.
- A SHA-256 timestamp endpoint.
- An authorized release operator or CI environment with access to the
  certificate.

The scripts accept:

| Variable | Purpose |
|---|---|
| `FLUX_SIGN_CERT_SHA1` | Certificate thumbprint in the Windows store. Preferred. |
| `FLUX_SIGN_CERT_PATH` | PFX file path, when store selection is unavailable. |
| `FLUX_SIGN_CERT_PASSWORD` | PFX password; only with `FLUX_SIGN_CERT_PATH`. |
| `FLUX_SIGN_TIMESTAMP_URL` | RFC 3161 timestamp URL. Defaults to DigiCert. |
| `FLUX_REQUIRE_SIGNING=1` | Fail if signing material is missing or invalid. |

### Procedure

1. Provision the certificate outside the repository. Never commit a PFX,
   password, token or certificate export.
2. Run:
   ```powershell
   $env:FLUX_SIGN_CERT_SHA1 = '<authorized thumbprint>'
   $env:FLUX_REQUIRE_SIGNING = '1'
   packaging/windows/build-windows.ps1 -Architecture x86_64
   ```
   Use `-Architecture arm64 -SkipInstaller` for the ARM64 portable candidate.
3. The script signs `flux.exe` and `flux-tui.exe` before ZIP creation and signs
   the installer after Inno Setup compilation.
4. It verifies each file with `signtool verify /pa /v` and
   `Get-AuthenticodeSignature`; any non-`Valid` result fails the build.
5. Run the verifier with signing enforced:
   ```powershell
   $env:FLUX_REQUIRE_SIGNING = '1'
   packaging/windows/verify-windows.ps1 -Architecture x86_64 -TestInstaller -TestGuiLaunch
   ```
6. Record the signer subject, timestamp, final SHA-256 checksums and installer
   lifecycle result. SmartScreen reputation still develops over time; signing
   does not guarantee immediate reputation.

CI does not inject signing secrets today. An authorized release pipeline must
map the variables from protected CI secrets into the Windows packaging job. Do
not enable `FLUX_REQUIRE_SIGNING` in ordinary push CI.

## macOS Developer ID and notarization

### Required material

- Apple Developer Program membership.
- Developer ID Application certificate installed in a CI keychain.
- A `notarytool` keychain profile or supported API-key credentials.
- Hardened-runtime signing; the current script uses `--options runtime`.
- No custom entitlements are currently required by Flux. Review this statement
  before adding network, audio-input, accessibility or automation entitlements.

### Procedure

1. Create and unlock an ephemeral CI keychain. Import the Developer ID
   certificate and private key. Do not print the certificate or password.
2. Store notary credentials with `xcrun notarytool store-credentials` under a
   protected profile name.
3. Build with:
   ```sh
   CODESIGN_IDENTITY='Developer ID Application: <authorized publisher>'
   NOTARY_PROFILE='<protected profile>'
   packaging/macos/build-macos.sh
   ```
4. The script:
   - signs `Flux.app` with hardened runtime and a secure timestamp;
   - verifies the signature;
   - submits the app ZIP with `notarytool --wait`;
   - staples and validates the ticket;
   - runs `spctl -a -vv`.
5. Verify:
   ```sh
   CODESIGN_IDENTITY='Developer ID Application: <publisher>' \
   NOTARY_PROFILE='<profile>' packaging/macos/verify-macos.sh
   ```
6. Confirm `codesign -dvv` reports a Developer ID authority, `stapler validate`
   succeeds and Gatekeeper accepts the bundle.
7. Publish the ZIP containing the stapled app. Before treating the DMG as a
   separately notarized distribution container, sign and notarize the DMG or
   validate the stapled app after mounting it. The current script notarizes and
   staples the app before creating the DMG, but does not separately notarize the
   DMG file.

CI currently does not inject macOS signing credentials. An authorized release
pipeline must provision the keychain and notary profile from protected secrets.

## Final release boundary

- Ordinary push/pull-request CI never signs, tags, creates a release or uploads
  a public binary.
- Packaging remains manual-dispatch and uploads private workflow artifacts.
- Signed artifacts must be generated only after an explicit release decision.
- Publish the exact checksums generated from the signed files, not pre-signing
  checksums.
