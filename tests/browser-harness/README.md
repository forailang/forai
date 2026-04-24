# Browser fixture harness

This folder contains the Playwright runner used by
`crates/fai-feature-tests` for fixtures with a `browser:` directive.

Setup:

```bash
npm install --prefix tests/browser-harness
npm run --prefix tests/browser-harness install-browsers
```

Run:

```bash
cargo test -p fai-feature-tests browser -- --nocapture
```
