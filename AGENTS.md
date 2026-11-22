# Instructions

## Go

- **Never use `go build`** — do not compile Go code into binaries under any circumstances
- Always run Go programs with `go run <path>`
- Use `go vet <path>` for static analysis
- Use `go fix <path>` for automated fixes
- Use `go test <path>` for tests
- Before `go run` if code is new: `go vet/fmt/fix/test`

This keeps the repo free of committed binaries.
It also ensures all Go is correct and run from source.

## Rust

- Use `cargo fmt`
- Use `cargo check` before `cargo run` or `cargo build`
- No Warnings
- cargo doc has been run on this repo
- search there first if you are not sure about dependency APIs
- cargo docs can be found here: `target/doc/`

## Git

- **Never run `git push`, `git pull`, or `git fetch`** — the user controls when code moves between local and remote
- **Never run destructive git commands** (`push --force`, `reset --hard`, `branch -D`, `clean -f`, `rebase`, etc.) under any circumstances
- Only create commits when the user explicitly asks (`commit`, `commit and push`, etc.)
- For any other git operation that mutates history or shared state, ask first

## Additional CLI Tools

ripgrep (rg)
hexyl
exa
uv
imagemagick (magick)
ffmpeg
curl
wget
grype
syft

## Python

Only use uv to install deps, and run python code.

uv is built from source and manages python.

I repeat: for python use `uv run python -`

## Commits

- No agent attribution in commits (no `Co-authored-by`, `Made-with`, or similar trailers/footers)
- Author/committer identity must be the human owner’s configured git user only

## Writing

- Prefer plain ASCII in docs, comments, commit messages, and replies
- Do not use uncommon symbols such as `§` (section sign); write "section 5.1" instead
- Avoid ornamental unicode (arrows like `→`, fancy dashes, emoji) unless the file already uses them for a clear purpose
