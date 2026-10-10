### Security

#### Bump `rustls` for RUSTSEC-2026-0285

`rustls` 0.23.40 is affected by RUSTSEC-2026-0285, "TLS 1.3 handshake messages
incorrectly accepted across encryption level boundaries" (severity 5.3,
medium). Updated to 0.23.45, which clears `cargo audit`.
