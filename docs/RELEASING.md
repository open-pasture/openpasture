# Releasing

A release is a tag push. `.github/workflows/release.yml` then builds, in parallel:

| Asset | Built on |
| --- | --- |
| `openpasture-macos.dmg` and `openpasture-<version>-macos-universal.dmg` (the same file) | macOS, universal (Apple Silicon + Intel), signed with Developer ID, notarized and stapled |
| `openpasture-server-linux-x64.tar.gz` | Ubuntu 22.04 x64 |
| `openpasture-server-linux-arm64.tar.gz` | Ubuntu 22.04 arm64 (native runner) |
| `openpasture-server-macos-arm64.tar.gz`, `openpasture-server-macos-x64.tar.gz` | macOS |
| `ghcr.io/open-pasture/openpasture:<version>` and `:latest` | native amd64 and arm64 runners, one multi-arch manifest |
| `openpasture-macos.app.tar.gz`, `.sig` and `latest.json` | the macOS job: the same notarized app, packed and signed for the in-app updater |
| `THIRD-PARTY-NOTICES` | every crate and web package in the builds, with its licence |
| `SHA256SUMS` | all of the above files |

Each tarball holds `openpasture`, `collar-sim` (the simulator), `LICENSE`, `NOTICE`, `THIRD-PARTY-NOTICES` and
a `README.md`. The Linux builds link glibc 2.35, so they run on Ubuntu 22.04+, Debian 12 and
Raspberry Pi OS bookworm. The macOS server binaries aren't signed or notarized (only the app
is); the server README covers the `xattr` step. The app carries the licence files in
`Contents/Resources`, and the Docker image in `/usr/share/doc/openpasture/`.

The first job, **Version and notices**, stops the release if the tag disagrees with the
version files, then writes `THIRD-PARTY-NOTICES` (`scripts/third-party-notices.py`: cargo-about
for the crates, `ui/node_modules` for the web packages) and takes the release notes from the
tag's section of `CHANGELOG.md`. The macOS job runs in the `release` environment: it waits for
a maintainer to approve it, and only it can read the signing secrets.

The GitHub release is published only when every build passes, and the Docker `latest` tag only
moves then too. Because the macOS file name never changes,
`https://github.com/open-pasture/openpasture/releases/latest/download/openpasture-macos.dmg`
always points at the newest app.

## In-app updates

The installed app reads
`https://github.com/open-pasture/openpasture/releases/latest/download/latest.json` about ten
seconds after launch and every six hours, and when you pick **openpasture > Check for
Updates…**. When a newer version is published it asks **Update** or **Later**; Update downloads
`openpasture-macos.app.tar.gz`, checks its signature against the public key in
`tauri.conf.json`, replaces the app in place, and restarts it. Later keeps the background check
quiet about that version until the next launch; the menu item still offers it. Debug builds
never check.

So publishing a release is all it takes: once the GitHub release is out, every installed app
that has the updater picks it up. Pre-releases don't move `releases/latest`, so apps never
offer them. 0.1.0 has no updater; people on it download the DMG once more.

The macOS job refuses to run without the Apple and updater secrets below: it fails at its first step and
names the missing ones. There is no unsigned fallback.

## Cut a release

1. `main` is green in CI.
2. On a branch, set the version and name the release in the changelog:

   ```
   git switch -c release/0.2.0
   scripts/set-version.py 0.2.0      # Cargo.toml, Cargo.lock, tauri.conf.json, ui/package.json
   ```

   In `CHANGELOG.md`, rename `## Unreleased` to `## 0.2.0 (YYYY-MM-DD)` and add a new empty
   `## Unreleased` above it. Append the release's new migrations to
   `crates/op-core/migrations/SHIPPED` under a `# v0.2.0` line
   (`cargo test -p op-core --test migrations -- --nocapture` prints the lines); from then on they
   can't be edited, and new migrations must be numbered above them. Commit ("Release 0.2.0"), open
   a pull request, and merge it once CI passes.

3. Tag the merge commit on `main` and push the tag:

   ```
   git switch main && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

4. Approve the `release` environment when the run asks (Actions > the run > **Review
   deployments**).

The tag must match the files: the first job runs `scripts/set-version.py --check "$tag"` and
stops the release if any version differs, so a release can't ship numbered differently from its
source. CI runs `scripts/set-version.py --check` on every pull request, so the files always agree
with each other. A tag with a suffix (`v0.2.0-rc.1`) is published as a pre-release: it moves
neither `releases/latest` nor the Docker `latest` tag; set the files to `0.2.0-rc.1` first.

Watch it with `gh run watch -R open-pasture/openpasture`. If a job fails, fix the cause and use
**Re-run failed jobs**; nothing is published until the last job. To redo a published release,
delete it and its tag (`gh release delete v0.2.0 --cleanup-tag -R open-pasture/openpasture`),
then tag again.

After the first release, open the package at
`https://github.com/orgs/open-pasture/packages/container/package/openpasture`, and under
**Package settings** set the visibility to **Public** (new packages start private). The
organisation must allow public packages (Organisation settings > Packages).

## Local builds

```
bun run desktop:release    # universal .app and .dmg, ad-hoc signed, not notarized
cargo build --release --target aarch64-apple-darwin -p op-cli -p collar-sim
scripts/third-party-notices.py                                                  # needs cargo-about
scripts/package-server.sh aarch64-apple-darwin openpasture-server-macos-arm64   # out/*.tar.gz
docker build -t openpasture .
```

Without `THIRD-PARTY-NOTICES` the tarball and image still build, but lack the notices; don't
hand those out. `cargo install --locked cargo-about --features cli` installs the generator.

## Apple signing, once

You need the Apple Developer Program membership, and the Account Holder role for step 1
(only the Account Holder can create Developer ID certificates). First sign in at
https://developer.apple.com/account and accept any pending agreements; notarization fails
while one is outstanding.

### 1. Developer ID Application certificate

1. On your Mac, open **Keychain Access**. Menu: **Keychain Access > Certificate Assistant >
   Request a Certificate From a Certificate Authority**. Enter your email and name, leave CA
   Email empty, choose **Saved to disk**, and save `CertificateSigningRequest.certSigningRequest`.
2. Go to https://developer.apple.com/account/resources/certificates/add, choose **Developer ID
   Application**, Continue. Pick the **G2 Sub-CA** profile, upload the request file, Continue,
   and **Download** `developerID_application.cer`.
3. Double-click the `.cer`. It is added to your **login** keychain, paired with the private key
   the request created.
4. Check it, and note the exact identity string and the team ID in brackets:

   ```
   security find-identity -v -p codesigning
   #  1) 3A1B...  "Developer ID Application: Your Name (AB12CD34EF)"
   ```

   The quoted string is `APPLE_SIGNING_IDENTITY`; `AB12CD34EF` is `APPLE_TEAM_ID` (also shown at
   https://developer.apple.com/account under Membership details).

### 2. Export the .p12

1. In Keychain Access, select the **login** keychain and the **My Certificates** tab.
2. Expand **Developer ID Application: Your Name (…)**. It must show a private key under it; if
   not, the certificate was installed on another Mac and has to be exported there.
3. Right-click the certificate (not the key) > **Export**, format **Personal Information
   Exchange (.p12)**, save as `developer-id.p12`, and set a strong password. That password is
   `APPLE_CERTIFICATE_PASSWORD`.

The workflow base64-decodes the secret, so it is stored base64-encoded (step 4 does this):

```
base64 -i developer-id.p12 | tr -d '\n' > developer-id.p12.b64
```

### 3. App Store Connect API key (for notarization)

1. Go to https://appstoreconnect.apple.com/access/integrations/api (**Users and Access >
   Integrations > App Store Connect API**). If asked, **Request Access** first (Account Holder).
2. Under **Team Keys**, click **+** (Generate API Key). Name it `openpasture notarization`,
   Access **Developer**, Generate.
3. **Download** `AuthKey_<KEYID>.p8`. Apple lets you download it only once; keep it safe.
4. Note the **Key ID** of the new key (`APPLE_API_KEY`) and the **Issuer ID** shown above the
   list (`APPLE_API_ISSUER`, a UUID).

### 4. Add the GitHub secrets

The secrets belong to the `release` environment, not the repository, so only the approved
macOS job can read them. Create the environment once (Settings > Environments > New
environment, `release`; add yourself under **Required reviewers**; under **Deployment branches
and tags** allow only tags matching `v*`).

With the GitHub CLI signed in as an admin of the repo (`gh auth status`), from the folder with
the files (every `gh secret set` below takes `--env release`):

```
REPO=open-pasture/openpasture

gh secret set APPLE_CERTIFICATE          -R $REPO --env release < developer-id.p12.b64
gh secret set APPLE_CERTIFICATE_PASSWORD -R $REPO --env release      # paste the .p12 password when prompted
gh secret set APPLE_SIGNING_IDENTITY     -R $REPO --env release --body "Developer ID Application: Your Name (AB12CD34EF)"
gh secret set APPLE_TEAM_ID              -R $REPO --env release --body "AB12CD34EF"
gh secret set APPLE_API_ISSUER           -R $REPO --env release --body "00000000-0000-0000-0000-000000000000"
gh secret set APPLE_API_KEY              -R $REPO --env release --body "KEYID12345"
gh secret set APPLE_API_PRIVATE_KEY      -R $REPO --env release < AuthKey_KEYID12345.p8
openssl rand -base64 32 | tr -d '\n' | gh secret set KEYCHAIN_PASSWORD -R $REPO --env release

gh secret list -R $REPO --env release
```

| Secret | What it is |
| --- | --- |
| `APPLE_CERTIFICATE` | The Developer ID Application `.p12`, base64 |
| `APPLE_CERTIFICATE_PASSWORD` | The password set when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: Your Name (TEAMID)`, exactly as `security find-identity` prints it |
| `APPLE_TEAM_ID` | The 10-character team ID |
| `APPLE_API_ISSUER` | App Store Connect API Issuer ID |
| `APPLE_API_KEY` | App Store Connect API Key ID |
| `APPLE_API_PRIVATE_KEY` | The contents of `AuthKey_<KEYID>.p8`; the workflow writes it to a file and sets `APPLE_API_KEY_PATH` |
| `KEYCHAIN_PASSWORD` | Any random string; protects the temporary keychain on the runner |

Then delete `developer-id.p12.b64` and keep `developer-id.p12` and the `.p8` somewhere safe
(a password manager). `GITHUB_TOKEN` covers the release and the Docker push; no other secret is
needed.

### 5. Updater signing key

The updates are signed with a minisign key, separate from Apple's. The public half is
`plugins.updater.pubkey` in `apps/desktop/src-tauri/tauri.conf.json`; installed apps accept
only archives signed by the private half. To make one:

```
bunx @tauri-apps/cli@2.12.4 signer generate -w updater.key -p "<password>" --ci
gh secret set TAURI_SIGNING_PRIVATE_KEY          -R $REPO --env release < updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD -R $REPO --env release --body "<password>"
```

and put the contents of `updater.key.pub` in `tauri.conf.json`. Keep `updater.key` and the
password with the Apple files. If they are lost, a new key needs a new public key in the app,
so everyone installed then has to download the DMG by hand once.

| Secret | What it is |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | The contents of `updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Its password |

Local builds don't make the update archive, so they need neither.

### Checking a signed build

```
spctl --assess --type execute -vv /Applications/openpasture.app      # "source=Notarized Developer ID"
xcrun stapler validate /Applications/openpasture.app
```

If notarization is rejected, the job log has the submission ID; with the same key:
`xcrun notarytool log <id> --key AuthKey_KEYID.p8 --key-id KEYID --issuer ISSUER`.
