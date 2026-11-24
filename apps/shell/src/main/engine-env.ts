// SEUOFFICE_WASM names the engine file for this process's worker threads
// (apps/*/src/main/engine-init.ts), but child processes inherit it too. On macOS
// the update swap reopens the app through `open`, so the new version starts with
// the value the previous version set: a hashed chunk inside the previous
// app.asar that the new one may not have (the update from 0.12.54 to 0.12.55
// stopped at startup on ENOENT). The main starts its engine from its own bundled
// file, so it drops an inherited value before any module reads it.
delete process.env.SEUOFFICE_WASM
