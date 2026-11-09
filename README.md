<div align="center">
  <h1>md</h1>

  <p><strong>Preview Markdown with kitty graphics</strong></p>

  <p>
    <img alt="License" src="https://img.shields.io/badge/license-MIT-green">
    <img alt="Rust" src="https://img.shields.io/badge/rust-1.85%2B-orange">
    <img alt="Edition" src="https://img.shields.io/badge/edition-2024-blue">
    <a href="https://crates.io/crates/md"><img alt="crates.io" src="https://img.shields.io/crates/v/md.svg"></a>
  </p>

  <p>
    <a href="#install">Install</a>
    &nbsp;·&nbsp;
    <a href="#quickstart">Quickstart</a>
  </p>
</div>

---

## Install

Requires [Rust](https://rustup.rs) **1.85+** and `~/.cargo/bin` on your `PATH`.

```bash
cargo install md
```

Verify:

```bash
md --help
```

<details>
<summary><strong>Build from source</strong> — for development or unreleased changes</summary>

```bash
git clone https://github.com/cesarferreira/md.git
cd md
cargo install --path . --locked
# or
make install-release
```

Debug install (faster compile, larger binary):

```bash
make install
```

Run without installing:

```bash
make build-release
./target/release/md
```

</details>

<a id="quickstart"></a>
## Quickstart

`md` renders a Markdown file into the terminal with the kitty graphics protocol. It works in kitty, Ghostty, WezTerm, and any other terminal that speaks that protocol.

```bash
md README.md
md < README.md
```

`j`/`k` scroll, click a link, drag and press `y` to copy, `+`/`-` zoom, `?` lists every key, `q` quits. Saving the file updates the preview, scrolls to the first block that changed, and marks it. `--theme dark|light|auto` follows the terminal background by default. Remote images such as badges are fetched in the background and cached, so the next open shows them immediately.

## License

MIT
