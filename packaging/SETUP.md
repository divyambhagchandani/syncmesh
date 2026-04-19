# Packaging & signing setup

Free signing + distribution routes for syncmesh. Follow these once per
project; after that every `v*` tag is packaged, signed, and published
automatically by `.github/workflows/release.yml`.

Both paths gracefully no-op when their secrets/vars aren't present, so
the release pipeline works today with unsigned artifacts and starts
signing + auto-tapping the moment you finish the steps below.

---

## 1. Homebrew tap (macOS + Linux, free)

`brew install` strips the macOS quarantine attribute, so unsigned
binaries distributed through a tap never trigger the Gatekeeper
warning. This replaces Apple's $99/year Developer ID for CLI tools.

### One-time setup

1. **Create a new public GitHub repo** named `homebrew-syncmesh`
   under your user (e.g. `divyambhagchandani/homebrew-syncmesh`).
   The `homebrew-` prefix is mandatory — Homebrew uses it to resolve
   `brew tap <user>/syncmesh`.
2. **Add the starter formula.** Copy
   [`packaging/homebrew/syncmesh.rb`](homebrew/syncmesh.rb) in this
   repo to `Formula/syncmesh.rb` in the new tap repo. (Homebrew
   requires the file live under `Formula/`.)
3. **Create a fine-grained Personal Access Token** with write access
   to *just* the `homebrew-syncmesh` repo. Save it as a secret named
   `HOMEBREW_TAP_TOKEN` in this repo's settings.
4. **Set a repository variable** (not secret) `HOMEBREW_TAP` to
   `<your-user>/homebrew-syncmesh`.

After that, every release tag triggers the `homebrew-bump` job, which
bumps the formula's version + sha256 in the tap repo via
[`dawidd6/action-homebrew-bump-formula`](https://github.com/dawidd6/action-homebrew-bump-formula).

### What users do

```sh
brew tap <your-user>/syncmesh
brew install syncmesh
```

---

## 2. SignPath.io (Windows, free for OSS)

SignPath sponsors free code-signing certificates for qualifying
open-source projects. A signed `syncmesh.exe` gets through Windows
SmartScreen after ~2 weeks of reputation accumulation; without it,
users see a scary warning on every download.

### Qualification criteria (as of SignPath's current policy)

- Public repo on GitHub.
- OSI-approved license (syncmesh is MIT OR Apache-2.0 ✅).
- 2FA enabled on the maintainer account.
- Reproducible builds (GitHub Actions with a locked toolchain ✅).
- Applied for via <https://signpath.org/apply>.

### One-time setup

1. **Apply.** Fill out the form at <https://signpath.org/apply>.
   Expect ~1–2 weeks for approval. Reference this repo URL; mention
   that the release workflow is already wired for their action.
2. Once approved, SignPath provisions an organization in their
   dashboard. From it, copy:
   - Organization ID → repository variable `SIGNPATH_ORG_ID`.
   - Project slug → repository variable `SIGNPATH_PROJECT_SLUG`
     (typically `syncmesh`).
   - API token → repository secret `SIGNPATH_API_TOKEN`.
3. In the SignPath dashboard, create a signing policy named
   `release-signing` (the workflow references that slug).

After that, the Windows build job uploads the unsigned `.exe`,
SignPath signs it asynchronously, and the signed artifact is
downloaded back into the build dir before the package step runs.

---

## 3. Linux — nothing to do

Linux binaries are distributed as `tar.gz` from GitHub Releases with
SHA256 checksums. No signing needed. A GPG signature (`.sig`) file
is nice-to-have; advertise your public key in the README if you add one.

---

## Smoke-testing the release pipeline

The release workflow triggers on `push` of a `v*` tag, or manually via
`workflow_dispatch`. To rehearse without cutting a public release:

```sh
git tag v0.1.0-rc1
git push origin v0.1.0-rc1
```

Check the workflow run:

- All five target builds should produce tarballs under artifacts.
- The `release` job creates a **draft** GitHub Release (won't go live
  until you publish it).
- If Homebrew + SignPath vars are set, their jobs run too; otherwise
  they skip silently.

If the rehearsal looks clean, delete the draft release and the tag,
then cut the real `v0.1.0` when you're ready.
