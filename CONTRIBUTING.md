# Contributing to HTTPTester

Thanks for your interest in contributing! This guide covers how to get started.

## Getting Started

1. **Fork** the repository on GitHub.
2. **Clone** your fork locally:
   ```bash
   git clone https://github.com/<your-username>/httptester.git
   cd httptester
   ```
3. **Create a branch** for your change:
   ```bash
   git checkout -b my-feature
   ```

## Development Setup

### Prerequisites

- **Rust toolchain** (stable) — install via [rustup](https://rustup.rs)
- **OpenSSL** development headers (required by the `web-push` crate)
  - macOS: `brew install openssl`
  - Ubuntu/Debian: `apt install pkg-config libssl-dev`
  - Windows: see the README for vcpkg instructions

### Running locally

1. Copy `.env.example` to `.env`.
2. Generate VAPID keys:
   ```bash
   npx web-push generate-vapid-keys
   ```
3. Paste the keys into `.env`.
4. Start the server:
   ```bash
   cargo run
   ```
5. Open `http://localhost:3000` in your browser.

### Frontend

The frontend is plain HTML, CSS, and vanilla JS — no build step required. Edit files in `frontend/` and reload the browser.

## Submitting Changes

1. Make sure the project builds without warnings:
   ```bash
   cargo build
   ```
2. Keep commits focused — one logical change per commit.
3. Push your branch and open a **Pull Request** against `main`.
4. Describe what your PR does and why.

## Code Style

- Rust code follows standard `rustfmt` formatting. Run `cargo fmt` before committing.
- Use `cargo clippy` to catch common issues.
- Frontend JS uses 2-space indentation and no framework dependencies.

## Reporting Issues

Open an issue on GitHub with:
- Steps to reproduce
- Expected vs. actual behavior
- Browser / OS if relevant

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Please be respectful in all interactions.
