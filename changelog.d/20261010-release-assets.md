### Fixed

#### Releases shipped cargo build debris instead of binaries

Every release from rc.1 to rc.20 carried about 500 files from cargo's
`target/` directory and no usable binary: the shared Rust release build
uploaded the directory wholesale, and `release-assets.yml` never ran because
a release created with `GITHUB_TOKEN` does not trigger `release: published`.
The shared build is now disabled for this repo, and the release workflow calls
`release-assets.yml` directly, attaching the linux-amd64 and linux-arm64
tarballs, `SHA256SUMS`, and build attestations. The broken releases were
deleted; their tags remain.
