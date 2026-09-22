# How does a maintainer release a new update to Rustrace?

Assume that the latest release is v0.1.0, and a fix should be released as v0.1.1. The release workflow (.github/workflows/release.yml) automatically publishes when you push a v* tag.

1. Change version = "0.1.0" to version = "0.1.1" under [workspace.package] in Cargo.toml, then refresh the lockfile and validate:

```
cargo check --workspace
./scripts/check-release-tag.sh v0.1.1
cargo test --test submit_cli --test retention_cli
```

2. Commit the fix and version bump:

```
git add Cargo.toml Cargo.lock [all other files]
git commit -m "Release Message"
```

3. Push the commit and release tag:

If a tag of the same name already exists in the local or remote repository, delete them first:

```
git push origin --delete v0.1.1
git tag -d v0.1.1
```

and then:

```
git push origin main
git tag -a v0.1.1 -m "Release Message"
git push origin v0.1.1
```

GitHub Actions then verifies macOS and Linux builds, generates latest.json, and publishes v0.1.1 as the latest release. A manual workflow run only validates; pushing the tag triggers publication.

Once it appears on GitHub Releases, students can quit Rustrace and run:

```
rustrace update
rustrace --version
```
