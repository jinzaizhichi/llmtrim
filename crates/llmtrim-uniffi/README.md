# llmtrim-uniffi

[UniFFI](https://mozilla.github.io/uniffi-rs/) bindings over [`llmtrim-core`] — one Rust
definition, idiomatic in-process bindings for **Python, Ruby, Swift and Kotlin**. The
compression runs natively in the caller's process (no server, no async).

## API

A deliberately flat surface over the engine:

```rust
fn compress(
    input: String,                 // a provider-shaped request body (JSON)
    provider: Option<Provider>,    // OpenAi | Anthropic | Google, or None to auto-detect
    preset: Option<String>,        // "aggressive" | "agent" | "code" | "rag" | "safe" | …
                                   // None = config from the environment / config file
) -> Result<CompressOutput, LlmtrimError>
```

`CompressOutput` carries the compressed `request_json`, the resolved `provider`/`model`,
the tokenizer label/exactness, and the before/after/frozen input-token counts. Embedders
that need the full rehydration plan or per-stage reports should depend on [`llmtrim-core`]
directly in Rust.

## Python

```bash
# Build a self-contained wheel (cdylib + generated glue):
crates/llmtrim-uniffi/scripts/build-wheel.sh --release
pip install target/wheels/llmtrim-*.whl
```

```python
import llmtrim_ffi as llmtrim, json

req = json.dumps({"model": "gpt-4o",
                  "messages": [{"role": "user", "content": "…"}]})
out = llmtrim.compress(req, llmtrim.Provider.OPEN_AI, "aggressive")
print(out.input_tokens_before, "->", out.input_tokens_after)
# send out.request_json to the provider
```

> **Why `build-wheel.sh` and not plain `maturin build`:** maturin's `bindings = "uniffi"`
> auto-packaging is sensitive to the maturin↔uniffi version pair. With maturin 1.14 +
> uniffi 0.31 it builds the native library into the wheel but omits the generated Python
> glue (empty package `__init__.py`). The script runs maturin, then injects the freshly
> generated bindings and repacks the wheel with valid RECORD hashes. Remove it once the
> auto path packages cleanly.

## Ruby / Swift / Kotlin

Generate from the same built library — no extra Rust:

```bash
cargo build --release -p llmtrim-uniffi
cargo run --bin uniffi-bindgen -p llmtrim-uniffi -- \
    generate --library target/release/libllmtrim_ffi.so --language <ruby|swift|kotlin> --out-dir out/
```

[`llmtrim-core`]: ../llmtrim-core
