# Vendored msoffice-crypto

Source: https://crates.io/crates/msoffice-crypto/0.1.0-rc.15

The upstream crate archive has SHA-256 `d5d6a4571f94b42a4910d244b00bc1fa268277c794d6ed633ad3c1285c824c37` and is licensed under MIT OR Apache-2.0. The corresponding licenses, upstream NOTICE, and the two fixtures included in the archive are retained here. The remainder of the upstream fixture corpus is not published in the archive.

Compatibility changes also follow officecrypto-tool 0.0.19. Its MIT license is preserved in `LICENSE-officecrypto-tool`.

The application uses this fork for Agile encryption and legacy binary Office
decryption. The application's `office_encryption` module implements Agile and
Standard decryption with OpenSSL, including the original provider's cipher and
digest selection and error classification.

The fork exposes a byte-oriented Agile writer, removes the upstream stream-size
ceiling in favor of checked, input-backed reads, and adapts the legacy BIFF5,
XOR, RC4 and CryptoAPI paths to officecrypto-tool behavior. PowerPoint retains
its persist directory instead of reproducing the original's directory count
change, which can produce a file PowerPoint rejects. Persist objects on either
side of the directory are decrypted up to the next object or metadata boundary.

No hosted build or verification has been performed on this fork.
