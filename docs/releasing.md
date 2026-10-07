# Releasing Tilt

The Release workflow (`.github/workflows/release.yml`) builds static Linux binaries for
x86_64 and arm64, checks their size and linkage, and runs both with `--version` and `--help`
before creating a GitHub release. It then verifies the downloadable assets and checksums,
tests the launchers, publishes `tilt-live` to npm with provenance, and installs the published
package to verify that it can download and start the released binary.

## npm authentication

In `tilt-live`'s npm package settings, configure a GitHub Actions trusted publisher:

- Organization or user: `cloudycotton`
- Repository: `tilt`
- Workflow filename: `release.yml`
- Environment: leave empty (the workflow does not use a GitHub environment)
- Allow direct publishing with `npm publish`

The workflow uses GitHub's OIDC identity. No `NPM_TOKEN` secret is required.
For a new npm package, publish its first version manually with `npm login` and
`npm publish --access public` from `npm/` (copy the root `LICENSE` there first), then add
the trusted publisher in the package settings.

## Ship a version

1. Bump the version in `Cargo.toml`, Tilt's entry in `Cargo.lock`, and `npm/package.json`.
2. Let the pull request's CI pass, then merge into `main`.
3. Check that the Release workflow succeeds and that GitHub and npm show the new version.

On Linux, verify the public installation:

```sh
npx --yes tilt-live --version
npx --yes tilt-live --help
```

Tilt requires Linux on x86_64 or arm64; npm installation on macOS or Windows is rejected.
Running the server also requires an X11 display (for example Xvfb).

If npm publishing fails after the GitHub release succeeds, fix the npm settings and use
Actions → Release → Run workflow. It skips an existing GitHub tag and npm version, so retrying
does not overwrite a release. A code change needs a new version to produce new binaries.
