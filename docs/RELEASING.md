# Release process

## Version convention

The public release identifier is a milestone, not a patch release:

- `v0.1` — first usable MVP.
- `v0.2` — next planned milestone.
- `v0.3` — following planned milestone.
- Continue with `vMAJOR.MINOR` for later milestones.

Use the exact same two-component value for the Git tag and GitHub Release name. Do not create tags or releases such as `v0.1.0`, `v0.1.1`, or prerelease variants.

Cargo requires a three-component SemVer package version. Keep that internal version in `Cargo.toml` (currently `0.1.0`); its major and minor components must match the public release tag. For example, Cargo package version `0.1.0` maps to public release `v0.1`, and package version `0.2.0` maps to `v0.2`. The release tag is deliberately not the literal Cargo package version.

## Creating a release

1. Complete the milestone and merge its implementation into `main`.
2. Update the Cargo package version's major/minor components for the milestone, retaining Cargo's three-component format.
3. Confirm CI is green on `main`.
4. Create and push the two-component tag:

   ```sh
   git tag v0.1
   git push origin v0.1
   ```

5. The Release workflow validates the tag against `Cargo.toml`, runs the Windows and macOS test suite (`platforms.yml`, about 25 minutes, because those platforms do not run on every commit), and only then creates a GitHub Release named exactly after the tag, with generated release notes. If the platform tests fail, no release is created; fix the problem, delete the tag, and tag again.

The workflow rejects tags with a patch component, prerelease suffix, or major/minor mismatch. It does not build or upload release binaries yet; binary/container distribution is a separate roadmap item.

## Validation

CI runs the release-policy script's self-tests on every pull request and push to `main`:

```sh
bash scripts/check-release-tag.sh --self-test
```

To validate a prospective tag locally without creating it:

```sh
bash scripts/check-release-tag.sh v0.1
```
