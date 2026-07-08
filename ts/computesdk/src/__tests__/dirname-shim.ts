// Workaround for @computesdk/test-utils@2.0.0: its ESM bundle references
// __dirname at module scope (dotenv side effect). Bare identifiers resolve
// through globalThis in ESM, so defining it here (before that package is
// imported) cures the ReferenceError; the .env lookup then no-ops.
(globalThis as { __dirname?: string }).__dirname ??= "/";
