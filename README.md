# supdoc

Keep documentation in step with the code it describes.

supdoc tracks links from documentation to code, such as `[text](src/lib.rs#symbol)`. It warns you when the
code behind a link changes, so the docs can be updated with it.

> **Status:** early development. Nothing is usable yet.

## Development

```bash
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```
