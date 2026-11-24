# Native model providers

| Panda provider | Native wire API | Environment credential |
| --- | --- | --- |
| `anthropic` | Anthropic Messages | `ANTHROPIC_API_KEY` |
| `openai-api` | OpenAI Responses | `OPENAI_API_KEY` |
| `google` | Gemini streamGenerateContent | `GEMINI_API_KEY` |
| `openai-codex` | ChatGPT Codex subscription | `:login openai-codex` |

Select a physical model with `:model PROVIDER MODEL`. The `openai` and `codex` aliases select the Codex subscription. `:models PROVIDER` refreshes the provider's list. The built-in API-key transports request up to 4,096 output tokens. Configure another limit or endpoint in `$PANDA_HOME/models.json`.

For a configured provider, set `api` to `anthropic-messages`, `openai-responses`, `google-generative-ai` or `openai-completions`. Supply the versioned base URL, such as `https://api.anthropic.com/v1`, `https://api.openai.com/v1`, or `https://generativelanguage.googleapis.com/v1beta`. Panda appends the native method path and sends the credential in that family's header. Signed reasoning metadata is scoped to the originating model and configured provider. When you switch families, Panda rebuilds tool calls and their results from the conversation.

## Virtual models

Register virtual models in the same `models.json`. In this example, high-thinking turns go to the Anthropic model and other text turns use the default model.

```json
{
  "virtualModels": {
    "auto": {
      "default": { "provider": "openai-api", "model": "gpt-4.1", "thinking": "default" },
      "efforts": {
        "high": { "provider": "anthropic", "model": "claude-sonnet-4-5", "thinking": "low" }
      },
      "image": { "provider": "google", "model": "gemini-2.5-flash" }
    }
  }
}
```

Choose `:model router auto`, then set `:thinking high` for that route. Panda evaluates the rules for each request, including tool continuations, retries and summaries. An exact thinking-level match takes priority. The image rule applies when the latest user message contains an attached image. The default handles remaining requests. A target inherits the selected thinking level unless it provides its own `thinking` setting. Use `"default"` to request the backing provider's default.

Targets must name physical models. Configure credentials on those providers. The session keeps the virtual selection on resume. Routed answers, Pi JSONL assistant entries and the usage ledger identify the backing provider and the model that responded. Where every target declares a context window, Panda uses the smallest target window for the virtual model.

## llama.cpp

Panda supports an independently run llama.cpp server through the existing `openai-completions` family. Register its `/v1` base URL and loaded model ID in `models.json`. The model is available to the picker, one-shot CLI and virtual routes. Set its context and output limits to match the server.

We evaluated Pi's optional `/llama` UI. Its router discovers GGUF models and exposes loading, unloading and model download. A shared server can hold several models and allocate context across slots, so model changes affect clients beyond the current Panda session. Panda uses the existing llama.cpp service only for inference; the operator retains control of server startup, GPU allocation, GGUF downloads and shutdown. [Pi's pinned llama.cpp guide](https://github.com/earendil-works/pi/blob/f472efdc1f310c7340579db447ef19201c3232ea/packages/coding-agent/docs/llama-cpp.md)
