# Publishing a tested preview

Windows packages are built and tested on pushes to `main`. A published package must correspond exactly to a successful `Windows preview` run.

1. Set the next preview version in the build workflow, commit the intended code and wait for the native Windows checks and artifact upload to finish.
2. Create and push the release tag on that run's **exact source commit**. Preserve existing tags and releases.
3. Add `docs/releases/<version>.md` with the tested behavior and remaining limitations.
4. Dispatch `Publish verified Windows preview` with the successful build's run ID and version.

The verifier checks the workflow, branch, outcome, source revision, existing tag, SHA-256 and archive metadata before publishing.
The publication job uses the existing tag and does not create one through the release API; tag creation and asset upload can have different credential scopes.
Use the installer verification option in `Windows preview` after a release is public. Keep official game-client results separate from headless protocol and executable checks.
