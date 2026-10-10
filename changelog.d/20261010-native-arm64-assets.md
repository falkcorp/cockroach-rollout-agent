### Fixed

#### The arm64 release build could not link OpenSSL

Cross-compiling `openssl-sys` for `aarch64` on the amd64 runner failed for lack
of an arm64 OpenSSL. Each architecture now builds natively, arm64 on
`ubuntu-24.04-arm`, and a final job writes `SHA256SUMS`, uploads, and attests.
