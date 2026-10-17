# Signing and notarizing the macOS binaries

The release workflow signs the macOS binaries with a **Developer ID Application** certificate and
notarizes them with Apple when the secrets below exist. Without them the binaries are published
unsigned (the install script and Homebrew still work; browser downloads then need
`xattr -d com.apple.quarantine local-brain`).

## 1. Developer ID Application certificate (`.p12`)

Only the Account Holder of the Apple Developer team can create this certificate type.

1. Xcode → Settings → Accounts → select your team → **Manage Certificates…** → **+** →
   **Developer ID Application**.
   (Or create it at https://developer.apple.com/account/resources/certificates with a CSR from
   Keychain Access → Certificate Assistant → Request a Certificate from a Certificate Authority.)
2. Keychain Access → **login** → **My Certificates** → right-click
   "Developer ID Application: <Your Name or Company> (TEAMID)" → **Export…** → save as
   `developer-id.p12` and set an export password.

## 2. Notarization credentials (pick one)

**A. App Store Connect API key (recommended, no 2FA prompts)**

1. https://appstoreconnect.apple.com → Users and Access → **Integrations** → App Store Connect API
   → **Team Keys** → **+**, role **Developer**.
2. Download `AuthKey_XXXXXXXXXX.p8` (only possible once) and note the **Key ID** and the
   **Issuer ID** shown above the key list.

**B. Apple ID with an app-specific password**

1. https://account.apple.com → Sign-In and Security → **App-Specific Passwords** → generate one.
2. Your Team ID is shown at https://developer.apple.com/account (Membership details).

## 3. Add the GitHub secrets

With the GitHub CLI on your Mac (`brew install gh`, then `gh auth login`), from any directory:

```sh
R=koztay/local-brain
base64 -i developer-id.p12 | gh secret set APPLE_CERTIFICATE_P12 -R $R
gh secret set APPLE_CERTIFICATE_PASSWORD -R $R          # prompts for the .p12 export password

# Option A (API key):
base64 -i AuthKey_XXXXXXXXXX.p8 | gh secret set APPLE_API_KEY_P8 -R $R
gh secret set APPLE_API_KEY_ID -R $R --body XXXXXXXXXX
gh secret set APPLE_API_ISSUER_ID -R $R --body 00000000-0000-0000-0000-000000000000

# Option B (Apple ID) instead of A:
# gh secret set APPLE_ID -R $R --body you@example.com
# gh secret set APPLE_APP_PASSWORD -R $R               # prompts for the app-specific password
# gh secret set APPLE_TEAM_ID -R $R --body ABCDE12345
```

Optional: `APPLE_SIGNING_IDENTITY` if the `.p12` contains more than one identity (use the full
name, e.g. `Developer ID Application: Example LLC (ABCDE12345)`).

Or add the same names in the browser: repository → Settings → Secrets and variables → Actions →
New repository secret. Afterwards delete the local `.p12`/`.p8` copies you no longer need.

## 4. Release

Run the **Release** workflow (or push a tag). The macOS jobs log `Notarization accepted`.
Check a downloaded binary on a Mac:

```sh
codesign -dv --verbose=2 local-brain     # Authority=Developer ID Application: …
codesign --verify --strict local-brain && echo signed
```
