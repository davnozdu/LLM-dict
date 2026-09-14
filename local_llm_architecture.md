# Local LLM Architecture for macOS Dictation App

## Goal

Implement an optional local text-correction backend for a macOS application written in Rust.

The local model must:

- Correct spelling, grammar, punctuation, capitalization, and obvious dictation errors.
- Support Russian, English, and Czech.
- Preserve the original meaning and language.
- Return only the corrected text.
- Run locally and use Apple Silicon acceleration.
- Be easy for the user to install from inside the application.
- Act as a fallback when cloud processing is unavailable.
- Optionally be used permanently by users who prefer local processing.

The application should not require the user to install Ollama, Python, Homebrew, or a separate AI application.

---

# Recommended Runtime

## Primary backend: llama.cpp embedded through Rust bindings

Use `llama.cpp` through a Rust binding such as `llama-cpp-2`, with the Metal backend enabled on macOS.

Reasoning:

- Native local inference library.
- No separate server is required.
- No Python runtime.
- Models can be distributed as GGUF files.
- Apple Silicon is well supported through ARM NEON, Accelerate, and Metal.
- The application can load the model directly into its own process.

The backend should be abstracted behind an internal interface so that the inference engine can be replaced later without changing application logic.

Suggested conceptual interface:

```rust
pub trait TextCorrector {
    fn correct(&mut self, text: &str) -> Result<String, CorrectorError>;
}
```

The actual implementation may be asynchronous and should not block the UI thread.

---

# Recommended Local Models

The application should offer three model choices.

Exact memory usage depends on context size, runtime buffers, batch size, Metal configuration, and the selected quantization. The values below are approximate planning targets rather than guaranteed limits.

## 1. Gemma 3 1B: Lightweight

Recommended for users who prioritize:

- Small download.
- Low memory usage.
- Fast startup.
- Fast local fallback.

Suggested quantization:

`Q4_K_M`

Approximate download size:

`~0.7–0.9 GB`

Approximate working memory:

`~1–2 GB`

Advantages:

- Small model download.
- Lowest expected memory use among the recommended options.
- Suitable for short dictation correction.

Trade-offs:

- Lower correction quality on complex punctuation and multilingual text.
- May be less reliable with difficult Czech grammar and complicated Russian sentences.

---

## 2. Qwen3 4B: Balanced

Recommended as the default high-quality local option.

Suggested quantization:

`Q4_K_M`

Approximate download size:

`~2.5 GB`

Approximate working memory:

`~3–5 GB`

Advantages:

- Strong multilingual capability.
- Better quality on Russian, English, and Czech.
- Better handling of longer or more complicated dictated sentences.
- Good compromise between model size and quality.

Trade-offs:

- Larger download.
- Higher memory use.
- Slower initial loading than Gemma 3 1B.

---

## 3. Gemma 4 E2B: Higher-quality local option

This model must be included as a third option.

Gemma 4 E2B is the smallest model in the Gemma 4 family, but it is still approximately a 5B-parameter model. It should therefore be treated as a heavier option than Qwen3 4B.

Suggested quantization:

`Q4_K_M`

Approximate download size:

`~3.1–3.4 GB`

Approximate working memory:

`~4–5 GB with a small context`

The exact figure depends heavily on runtime configuration.

Advantages:

- Newer model family.
- Potentially stronger text understanding and correction quality.
- Good candidate for users willing to spend more memory for quality.
- Still practical on Apple Silicon Macs with sufficient Unified Memory.

Trade-offs:

- Larger download than Qwen3 4B.
- Higher memory consumption.
- Slower model loading.
- Probably unnecessary for users whose only requirement is simple punctuation and spelling correction.

Important implementation note:

Do not load Gemma 4 E2B automatically by default. It should be an optional model selected by the user.

---

# Suggested Model Selection UI

Example:

## Local AI Model

### Lightweight
**Gemma 3 1B**

Download: approximately 0.8 GB  
Memory: approximately 1–2 GB

Best for low memory usage and fast local processing.

[Install]

---

### Balanced
**Qwen3 4B**

Download: approximately 2.5 GB  
Memory: approximately 3–5 GB

Better multilingual correction and more reliable punctuation.

[Install]

---

### Advanced
**Gemma 4 E2B**

Download: approximately 3.1–3.4 GB  
Memory: approximately 4–5 GB

Higher-quality local processing at the cost of additional memory.

[Install]

The application should clearly label memory figures as approximate.

---

# Processing Modes

Do not use only a simple "internet available / internet unavailable" check.

A device may have internet access while the cloud service itself is unavailable because of:

- DNS failure.
- API timeout.
- Authentication failure.
- Backend outage.
- Rate limiting.
- Server error.

Instead, the application should make the actual cloud request and fall back to local processing when appropriate.

Provide three processing modes.

## 1. Cloud Only

Always use cloud processing.

If cloud processing fails, return an error.

The local model is not used automatically.

---

## 2. Automatic

Recommended default mode.

Flow:

```text
Text request
    |
    v
Try cloud processing
    |
    +-- Success --> Return cloud result
    |
    +-- Network/service failure
             |
             v
      Local model installed?
             |
        +----+----+
        |         |
       Yes        No
        |         |
        v         v
  Load if needed  Return error
        |
        v
  Local correction
```

Important:

Fallback should occur for actual transport/service failures, not merely because Wi-Fi is disconnected.

Do not automatically fall back for every API error.

Examples:

- Timeout: fallback allowed.
- Network unavailable: fallback allowed.
- HTTP 5xx: fallback allowed or configurable.
- HTTP 429: fallback may be configurable.
- Invalid API key: do not silently fall back unless explicitly desired.
- Invalid request: do not fall back.

---

## 3. Always Local

Never send text to the cloud.

Always process through the selected local model.

This mode should be useful for:

- Offline usage.
- Privacy-sensitive users.
- Users who want predictable local behavior.

---

# Model Loading Policy

Separate these two concepts:

1. Model installed on disk.
2. Model currently loaded into Unified Memory.

A model can be installed without permanently consuming RAM.

## Lazy Loading

In Automatic mode:

```text
Cloud processing works
    |
    v
Local model remains unloaded
    |
    v
No model memory consumption beyond minimal metadata
```

When cloud processing fails:

```text
Cloud request fails
    |
    v
Load local model
    |
    v
Process request locally
```

This reduces normal memory consumption.

---

## Keep Model Ready

Provide an optional toggle:

`Keep local model loaded in memory`

When enabled:

- Load the selected model in the background.
- Keep the inference context alive.
- Subsequent requests have lower latency.
- Memory usage remains higher.

When disabled:

- Load only when required.
- Optionally unload after an inactivity timeout.

Suggested inactivity timeout:

`5–15 minutes`

The exact value can be configurable later.

---

# Suggested Settings Structure

## Processing Mode

- Cloud only
- Automatic
- Always local

## Local Model

- No model installed
- Gemma 3 1B
- Qwen3 4B
- Gemma 4 E2B

## Memory Behavior

- Load only when needed
- Keep model ready in memory

## Optional Advanced Settings

- Unload after inactivity
- Inactivity timeout
- Context size

For this application, use a relatively small context by default because dictation correction normally does not require 32K or 128K context.

Suggested initial context target:

`2048–4096 tokens`

A smaller context reduces memory usage and initialization overhead.

---

# Model Installation UX

The user should install a model with one action.

Example flow:

```text
User clicks "Install"
        |
        v
Download model in background
        |
        v
Show progress
        |
        v
Download to temporary file
        |
        v
Verify file size and checksum
        |
        v
Atomically move to final model directory
        |
        v
Model available
```

Suggested storage location:

```text
~/Library/Application Support/<AppName>/Models/
```

Example:

```text
Models/
├── gemma-3-1b-q4.gguf
├── qwen3-4b-q4.gguf
├── gemma-4-e2b-q4.gguf
└── models.json
```

Never treat a partially downloaded file as an installed model.

Use a temporary extension such as:

```text
model.gguf.download
```

Rename or atomically move it only after verification succeeds.

---

# Download Architecture

The application should manage model downloads itself.

Recommended behavior:

- Download in a background task.
- Support progress reporting.
- Support cancellation.
- Support resume if practical.
- Verify integrity after download.
- Record installed model metadata.
- Detect corrupted or incomplete files.
- Allow model deletion from application settings.

The application should not require the user to manually download a GGUF file.

The model registry should be versioned.

Example conceptual metadata:

```json
{
  "id": "qwen3-4b-q4",
  "display_name": "Qwen3 4B",
  "file_name": "qwen3-4b-q4.gguf",
  "version": "1",
  "download_url": "...",
  "sha256": "...",
  "download_size_bytes": 2500000000
}
```

Use an HTTPS source controlled or explicitly approved by the application.

Do not hardcode mutable third-party "latest" URLs without integrity verification.

---

# Text Correction Prompt Requirements

The model should receive a strict instruction.

Conceptual prompt:

```text
Correct spelling, grammar, capitalization and punctuation.

Preserve the original meaning, terminology and language.

Do not translate the text.
Do not rewrite the text stylistically.
Do not add information.
Do not remove information unless it is clearly a dictation error.
Do not explain corrections.

Return only the corrected text.
```

The application should test the prompt against:

- Russian.
- English.
- Czech.
- Mixed-language technical text.
- Product names.
- Brand names.
- Model numbers.
- Sentences with missing punctuation.
- Speech-recognition errors.

Examples relevant to the application domain:

- MacBook model names.
- iPhone and Samsung product names.
- Technical terms.
- English product names embedded in Czech or Russian sentences.

---

# Performance Strategy on Apple Silicon

The target hardware includes Apple Silicon Macs such as M2.

The implementation should:

- Build for `aarch64-apple-darwin`.
- Use the Metal backend.
- Avoid launching an external inference server.
- Keep inference inside the application process.
- Use an appropriate number of GPU layers supported by the selected runtime.
- Test the selected model on actual Apple Silicon hardware.

The runtime should not force CPU-only inference on Apple Silicon unless required for compatibility.

---

# Recommended Initial Defaults

## Default processing mode

`Automatic`

## Default local model recommendation

`Gemma 3 1B`

Reason:

It minimizes download size and memory consumption.

## Recommended quality option

`Qwen3 4B`

Reason:

It is the most balanced choice between memory consumption and multilingual correction quality.

## Advanced quality option

`Gemma 4 E2B`

Reason:

It provides a newer and larger model option for users willing to accept higher memory consumption.

## Default memory policy

`Load only when needed`

## Optional performance mode

`Keep model ready in memory`

---

# Proposed Internal Architecture

```text
                 ┌───────────────────┐
                 │ Text Correction   │
                 │ Request           │
                 └─────────┬─────────┘
                           │
                           v
                 ┌───────────────────┐
                 │ Processing Router │
                 └─────────┬─────────┘
                           │
          ┌────────────────┼────────────────┐
          │                │                │
          v                v                v
      Cloud Only       Automatic       Always Local
          │                │                │
          v                v                v
      Cloud API       Try Cloud       Local Backend
                           │                │
                    ┌──────┴──────┐         │
                    │             │         │
                 Success        Failure     │
                    │             │         │
                    v             v         │
                  Return      Local Backend │
                                  │         │
                                  └────┬────┘
                                       │
                                       v
                              Model Manager
                                       │
                           ┌───────────┼───────────┐
                           │           │           │
                           v           v           v
                       Gemma 3      Qwen3      Gemma 4
                         1B          4B          E2B
```

---

# Implementation Priority

## Phase 1

Implement:

- Embedded local inference backend.
- One GGUF model.
- Manual installation.
- Always Local mode.
- Basic lazy loading.
- Metal acceleration.

Recommended first model:

`Gemma 3 1B Q4`

## Phase 2

Add:

- Qwen3 4B.
- Gemma 4 E2B.
- Model selection UI.
- Download manager.
- Automatic cloud fallback.

## Phase 3

Add:

- Keep model loaded toggle.
- Automatic unload after inactivity.
- Performance telemetry.
- Local benchmark on first use.
- Optional model recommendation based on available Unified Memory.

---

# Required Testing

Before finalizing the model choices, benchmark all three models on a real MacBook Pro M2 using the same runtime and settings.

Measure:

- Model download size.
- Time to load model.
- Unified Memory usage after loading.
- Time to first token.
- Total correction latency.
- Tokens per second.
- Energy impact.
- Correctness on Russian.
- Correctness on English.
- Correctness on Czech.
- Accuracy on mixed-language technical text.
- Frequency of unwanted rewriting.
- Frequency of meaning changes.

Use at least 50–100 realistic correction examples.

Do not select the default model solely from parameter count or synthetic benchmarks.

---

# Final Recommendation

Implement all three local options:

1. **Gemma 3 1B Q4** for minimum memory and fast deployment.
2. **Qwen3 4B Q4** as the balanced multilingual option.
3. **Gemma 4 E2B Q4** as the larger, higher-resource option.

The default application behavior should be:

```text
Automatic mode
    |
    +-- Cloud works -> use cloud
    |
    +-- Cloud unavailable -> use installed local model
    |
    +-- No local model installed -> show normal cloud error
```

The local model should remain installed on disk but unloaded from memory until needed unless the user explicitly enables:

`Keep local model ready in memory`

This architecture gives the user a simple experience while avoiding permanent memory consumption from a model that may only be needed as an offline fallback.
