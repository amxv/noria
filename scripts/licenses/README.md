# Reviewed upstream license sources

Most license texts are read directly from the crates selected by `Cargo.lock`.
The objc2 workspace publishes crates without copying its root `LICENSE.md` into
each package. `sources.json` maps those exact crate versions to the licensing
notice at the source commit recorded in `.cargo_vcs_info.json`.

That upstream notice links the MIT license rather than embedding its text.
`MIT.txt` is the linked license's standard text from SPDX license-list-data
v3.26.0; generated notices also preserve each crate's upstream author metadata.
No copyright ownership or years have been inferred or invented.

When updating these crate versions, review their new upstream license notice
and update the exact version/commit mapping. Unknown missing license texts fail
release generation rather than silently dropping an attribution.
