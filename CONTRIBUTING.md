# Contributing to LoomGate

Thank you for contributing to **LoomGate**! We welcome bug reports, protocol codecs, client profiles, middleware plugins, and UI improvements.

## Core Engineering Principles

1. **Lossless Canonical IR**: Never bypass Canonical IR with raw passthrough or silently strip `cache_control`, `thinking/signature`, `thought_signature`, or `strict` tool schemas.
2. **Explicit Transparent Routing**: `Lock` mode must never silently downgrade models. Every inference response must carry transparent headers (`X-Gateway-Actual-Model`, `X-Gateway-Channel-Id`, `X-Gateway-Route-Mode`, `X-Gateway-Sticky-Hit`).
3. **Local-First Privacy**: Never transmit raw prompt or response bodies to any cloud endpoint. Never commit `config.local.toml`, `.db`, or credentials.
4. **Zero Hardcoding**: Prefer declarative registries (e.g., `ClientProfile`, `[model_pricing]`, `[middleware]`) over vendor-specific branches.

## Local Development Setup

### Prerequisites
- Rust stable toolchain (`rustc`, `cargo`, `clippy`, `rustfmt`)
- Node.js >= 20 & npm
- Python >= 3.10 (for `tests/mock_upstream.py` and `tests/conformance_suite.py`)
- Linux Tauri v2 builds also require: `libwebkit2gtk-4.1-dev`, `libappindicator3-dev`, `librsvg2-dev`, `patchelf`, `libssl-dev`, `libxdo-dev`.

### Windows Cache Isolation Tip
Keep `CARGO_TARGET_DIR`, `CARGO_HOME`, and `TEMP`/`TMP` on a non-system drive (e.g., `D:` or `E:`) to avoid filling `C:`:
```powershell
$env:CARGO_TARGET_DIR = "E:\LoomGate\target"
$env:CARGO_HOME = "D:\cargo"
```

## Mandatory Verification Gates (Before Opening a PR)

Every PR must pass all five local verification gates:

```bash
# 1. Zero-warning Clippy across all workspace crates & test targets
cargo clippy --workspace --all-targets -- -D warnings

# 2. Full Rust workspace unit tests
cargo test --workspace

# 3. Unified Desktop / Web Console TypeScript & Vite build
cd apps/desktop && npm ci && npm run build && cd ../..

# 4. Official npm SDK & CLI tests
cd packages/loomgate && npm test && npm pack --dry-run && cd ../..

# 5. Full 18/18 End-to-End Conformance Suite
cargo build -p gateway-daemon -p cloud-control-plane
# Start tests/mock_upstream.py, cloud-control-plane, and gateway-daemon (with LOOMGATE_WEB_DIST=apps/desktop/dist), then:
python3 tests/conformance_suite.py
```

## Pull Request Guidelines

- Follow Conventional Commits (`feat(...)`, `fix(...)`, `docs(...)`, `chore(...)`, `test(...)`).
- Include unit tests in the relevant crate and an end-to-end test in `tests/conformance_suite.py` for new API or protocol behavior.
- Double-check `git status` before pushing to verify no `config.local.toml`, `.runtime/`, or temporary artifacts are staged.
