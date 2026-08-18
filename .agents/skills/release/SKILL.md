---
name: release
description: Prepare and publish a cloudflare-dns-operator release. Use when asked to release the project, bump its version, create a release commit and Git tag, or build and push its versioned and latest Docker images.
---

# Release

Create one release from a clean working tree. Keep the Cargo package version, Nix package version, Git tag, and Docker tag aligned.

Run project commands from the repository root through the default Nix development shell.

## Select the version

- Use the exact semantic version requested by the user.
- If the user requests a major, minor, or patch bump, calculate it from the version in `Cargo.toml` with `semver`.
- Default to a patch bump when the user does not select a version or bump type.
- Use `X.Y.Z` in project files and Docker tags. Use `vX.Y.Z` for the commit message and Git tag, matching the existing repository history.

## Prepare the release

1. Run `git status --short`. Stop if the working tree contains any change, including untracked files. Do not include unrelated work in a release.
2. Confirm that `HEAD` is on a branch and that `refs/tags/vX.Y.Z` does not exist. Never move or replace an existing release tag.
3. Update the package version in `Cargo.toml` and `nix/package.nix`.
4. Run `nix develop -c cargo check` to update `Cargo.lock`.
5. Replace `cargoHash` in `nix/package.nix` with `lib.fakeHash`.
6. Run `nix develop -c nix build '.#image' --no-link`. Copy the reported actual hash into `cargoHash`.
7. Run the image build again and require it to pass.

## Verify and commit

1. Run `nix develop -c just test`.
2. Run `git diff --check`.
3. Inspect the full diff. Require exactly these changed files:
   - `Cargo.toml`
   - `Cargo.lock`
   - `nix/package.nix`
4. Confirm that all three files contain the selected package version and that `cargoHash` is not `lib.fakeHash`.
5. Stage only those three files.
6. Create the commit with `git commit -m "vX.Y.Z"`.
7. Create a lightweight tag with `git tag "vX.Y.Z"`.
8. Confirm that the tag resolves to `HEAD` and that the working tree is clean.

## Publish the Docker image

1. Run `nix develop -c just docker-push X.Y.Z`. Pass the version explicitly so the recipe does not infer it.
2. Require both `robertkrahn/cloudflare-dns-operator:X.Y.Z` and `robertkrahn/cloudflare-dns-operator:latest` to push successfully.
3. Report the commit, Git tag, Docker tags, and verification results.

Do not push the Git branch or tag unless the user asks. Do not amend commits, force-push, delete tags, or discard changes. If Docker publishing fails after the commit and tag exist, preserve them and report the failed command and error.
