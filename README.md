# Gemini Terminal

A small terminal app for sending prompts to Google Gemini and reading the response.

## Install

Install for the current user with Cargo:

```sh
cargo install --path .
```

This installs `gemini-terminal` in Cargo's user bin directory, normally `~/.cargo/bin`. Ensure that directory is on your `PATH`.

To build a standalone release binary instead:

```sh
cargo build --release
```

The binary is `target/release/gemini-terminal` and can be installed under `/usr/local/bin` by an administrator or copied into a user bin directory.
For a user-local install without administrator privileges:

```sh
install -Dm755 target/release/gemini-terminal "$HOME/.local/bin/gemini-terminal"
```

## Configure

Create the private XDG config file and edit it:

```sh
gemini-terminal --init
${EDITOR:-vi} "${XDG_CONFIG_HOME:-$HOME/.config}/gemini-terminal/config.toml"
```

The file is created with owner-only permissions. Its contents look like this:

```toml
api_key = "YOUR_GEMINI_API_KEY"
model = "gemini-3.6-flash"
models = ["gemini-3.5-flash-lite", "gemini-3.6-flash"]
```

`model` selects the startup model. The app refreshes `models` from Google's API when first run and then no more than once every three days. The fetched list and `models_updated_at` timestamp are saved in the config. Only models that support `generateContent` are listed. If the refresh is unavailable, the last saved model list remains usable. See `gemini.example.toml` for the initial template. Use `--config PATH` to load a config from a custom location.

## Run

```sh
cargo run
```

To load a config file from another location:

```sh
cargo run -- --config /path/to/config.toml
```

Use `gemini-terminal --model gemini-3.5-flash-lite` to choose and save a startup model. The app uses the normal terminal scrollback: enter a prompt at `Prompt:`, then the reply appears at `Response:`, with a blank line between turns. Press F2 to show a numbered list of available models, type its number, and press Enter. The selected model is saved to the config. Ctrl+C or Esc quits while entering a prompt.

## Tests

Run offline tests with `cargo test`. To make a real API request and verify response text, point the ignored live test at a valid config file:

```sh
GEMINI_TEST_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/gemini-terminal/config.toml" \
GEMINI_TEST_MODEL=gemini-3.5-flash-lite \
cargo test gemini_live_request_returns_text -- --ignored
```