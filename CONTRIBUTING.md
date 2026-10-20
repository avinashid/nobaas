# Contributing

1. Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
2. Work on a branch named `<area>/NB-XXX-slug`, where `NB-XXX` is the task or issue number.
3. Run the gate before opening a PR:

   ```sh
   cargo test --workspace --release
   cargo clippy --workspace
   ./packages/js/build.sh && node packages/js/test/node_pair.mjs
   ```

4. Keep `nobaas-core` sans-IO and deterministic, and keep the wasm build free of imports.
5. Every performance number in the docs must be reproducible from the repo, or labelled theoretical with its
   formula.
