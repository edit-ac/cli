# Immutable shared Rust sources

This minimal snapshot comes from reviewed core revision `1a57fa527b28862865ca64c1fec170103d456af0`, qualified by `architecture/evidence/extension-library/receipt.json` and the integration ledger. It includes only the two consumed crates, their source mapper dependencies and Apache-2.0 license. `provenance.json` binds every original byte to its upstream path. The only source overlay narrows the root workspace members; no semantic implementation is modified.

Vendoring keeps the shared interchange API available without publishing the local revision, fetching an unavailable commit, or depending on a sibling repository. Cargo.lock pins all external dependencies. Do not edit these sources; replace with a separately qualified immutable upstream snapshot when updating. This is a native CLI dependency artifact, not a core or registry release claim.
