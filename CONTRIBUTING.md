# Contributing

1. Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [tasks/README.md](tasks/README.md).
2. Pick or open a task (`tasks/backlog/NB-XXX-*.md`). Work on a branch named `<area>/NB-XXX-slug`.
3. Run the gate before opening a PR:

   ```sh
   cargo test --workspace --release
   cargo clippy --workspace
   ./packages/js/build.sh && node packages/js/test/node_pair.mjs
   ```

4. Keep `nobaas-core` sans-IO and deterministic, and keep the wasm build free of imports.
5. Every performance number in the docs must be reproducible from the repo, or labelled theoretical with its
   formula.
