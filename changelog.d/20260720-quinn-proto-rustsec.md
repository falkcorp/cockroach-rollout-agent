### Security

#### Bump `quinn-proto` for RUSTSEC-2026-0185

`quinn-proto` 0.11.14 is affected by RUSTSEC-2026-0185, "Remote memory exhaustion
from unbounded out-of-order stream reassembly" (severity 7.5, high). Updated to
0.11.16, which clears `cargo audit`.
