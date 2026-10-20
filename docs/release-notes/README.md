# Release notes

One file per release, `v<x.y.z>.md`, written for users. When the file
exists the release workflow uses it as the GitHub release body; when it
does not, GitHub's generated notes are used instead.

To release: bump `version` in the workspace `Cargo.toml`, commit to main,
then tag `v<x.y.z>` and push the tag.
