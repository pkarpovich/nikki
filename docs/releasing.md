# Releasing

A release is a tag. Everything after the tag is CI: `.github/workflows/release.yml`
builds for `aarch64-apple-darwin`, checks the embedded `Info.plist` survived, signs
with the Developer ID, publishes a GitHub Release with the tarball and its checksum,
and rewrites `Formula/nikki.rb` in `pkarpovich/homebrew-apps`.

## Steps

1. Bump `version` in `Cargo.toml` and let `cargo` update `Cargo.lock`. Do it in a
   pull request together with whatever is being released - CI refuses a tag whose
   version disagrees with the crate.
2. Merge the pull request.
3. Tag the merge commit on `main` and push the tag:

   ```sh
   git checkout main && git pull
   git tag -a v0.1.1 -m "nikki 0.1.1"
   git push origin v0.1.1
   ```

   Tags are annotated. Re-pushing a tag re-runs the release and replaces a
   published tarball with one that has a different checksum, so a mistake gets a
   new version rather than a rewritten one.
4. Watch the run: `gh run watch $(gh run list --workflow=release.yml -L1 --json databaseId -q '.[0].databaseId')`.
5. Replace the generated release notes. The workflow publishes with
   `generate_release_notes: true`, which is only a list of merged pull requests:

   ```sh
   gh release edit v0.1.1 --title "nikki 0.1.1" --notes-file notes.md
   ```

## Verifying

```sh
curl -sfL https://raw.githubusercontent.com/pkarpovich/homebrew-apps/main/Formula/nikki.rb
brew update && brew upgrade nikki && brew services restart nikki
nikki --check-config
tail -f "$(brew --prefix)/var/log/nikki.log"
```

The `sha256` in the formula has to match `checksums.txt` on the release. After a
restart the log carries one `nikki started` line and records resume within a tick.

## Signing locally

```sh
./scripts/build-signed.sh GGG699AY79
```

The team id is the parenthesised suffix of a `Developer ID Application` identity in
`security find-identity -v -p codesigning`. The script signs with the same
identifier, hardened runtime and timestamp the workflow uses, so a locally built
binary and a released one carry the same identity and share the same TCC grants.

## Secrets

The workflow reads four repository secrets: `MACOS_CERT_P12_BASE64` and
`MACOS_CERT_PASSWORD` (the exported Developer ID Application certificate),
`MACOS_TEAM_ID`, and `HOMEBREW_TAP_TOKEN` (a fine-grained token with write access
to `pkarpovich/homebrew-apps` and nothing else). They are the same four `nhop`
uses and are stored in 1Password.

`BUNDLE_ID` must never change. macOS ties both the Accessibility grant and the
Automation grant for Dia to the signing identity plus that identifier, so a
different value loses both: capture keeps running, but every window title goes
null and no browser tab is ever read again.
