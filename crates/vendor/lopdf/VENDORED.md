# lopdf

Source: lopdf 0.42.0 from crates.io, MIT license in `LICENSE`. The registry archive SHA-256 is `25aab26d99567469098e64a02f42679f8965c6401263eefa31d8f2dcc37a221c`.

The fork's strict mode is diagnostic only. It rejects invalid xref sizes, forward or repeated previous-xref offsets, hybrid xrefs, encrypted input and prefixes before the PDF header. It propagates errors for indirect objects, object streams and deferred stream content, and checks xref object identities and compressed-object indices. The default non-strict loader is unchanged.

Strict diagnostics are not a product signing gate. The original pdf-lib signing path accepts some malformed input that strict mode rejects. A compatible independent interpretation and validation of the requested incremental operation is still required.

The fork also provides opt-in raw encrypted loading and checked encryption/decryption. These retain the original security dictionary until explicit authentication, recover legacy owner keys, process stream dictionary strings and fail on decryption or required object-stream errors. Existing ordinary loading and encryption APIs retain their original policy.
