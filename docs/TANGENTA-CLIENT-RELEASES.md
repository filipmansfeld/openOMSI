# Tangenta client releases

This branch retains the Tangenta server policy, network behaviour, live walkers and launcher map selection changes from upstream base `8842610fca44a0979307597d9946af1b6e606717` (`v0.1.1098`). The baseline files were verified against the local Tangenta build receipts before being copied into this isolated worktree.

The client updater uses only [filipmansfeld/openOMSI](https://github.com/filipmansfeld/openOMSI/releases). It ignores official engine tags, server archives, drafts and prereleases. Both manual and automatic installation require the exact Tangenta client filename, a download and release page belonging to that repository, a nonzero file size and a valid GitHub SHA-256 asset digest. `OMSI_UPDATE_URL` cannot change this channel.

Versions have the form `0.1.1098-tangenta.2`. Increment the revision for each release on the same engine base. The original unnumbered `0.1.1098-tangenta` version is revision zero. `TANGENTA_VERSION` stamps local builds unless `OPENOMSI_VERSION` explicitly supplies a version.

For Windows x64 the release asset must be named `openOMSI-0.1.1098-tangenta.2-client-windows-x64.zip`. The ZIP contains the executable, launcher, runtime DLLs, license and build information at its root. Do not wrap those files in another directory or include the player's `profile`, `content`, settings, credentials or server deployment files. Automatic installation replaces files in the running executable's directory and restarts the launcher; the player's profile and content directories stay outside that directory.

The `Tangenta client draft release` workflow runs only on Tangenta tags in the owner's fork. It builds Windows x64, checks the updater and network tests, packages the flat client archive and creates a draft release. Publishing requires reviewing that draft and clearing both the draft and prerelease flags. A draft is never offered to players. Push the reviewed source branch and its matching `v0.1.1098-tangenta.2` tag to the `fork` remote when preparing the next release; keep the tag and `TANGENTA_VERSION` equal.

The private client launcher wrapper should enable updates only after it points to a tested Tangenta channel build. Remove its child-process `OMSI_NO_UPDATE=1` assignment and enable `update_check` and `update_auto` in the isolated player profile when automatic installation is desired. Keep the existing profile and content paths. The channel-aware executable is required before enabling updates; older Tangenta executables still use the official updater.

As of the initial integration, the fork had no published releases or tags. An empty release list is treated as up to date and never causes a request to the official release repository.
