# Example: one OpenAI-compatible gateway, two model families

A `~/.mantra/models.toml` for a gateway that fronts several open models (the aliases here are
the ones a real build of an Android app was run with), and two patterns that use them:

- `mantra-default-riti.toml` — the built-in default pattern with its roles mapped to the gateway's
  models: the big roles (planner, worker-big, qa-heavy) on one family, the small ones (manager,
  worker-small, qa) on the faster family. Copy it to `~/.mantra/patterns/mantra-default.toml` to
  make it the default.
- `riti-split.toml` — the previous heavy flow (separate security sweep, review every 3 minutes),
  kept as `riti-split`.

These files were written for 0.6, when every gateway went through Codex (`kind = "codex"`, the
Responses API); they still load, and run on Codex. On 0.8 you would paste the gateway's key in
`/models` instead — Mantra adds it as a bija provider and lists its models — and point the
pattern's roles at those aliases.

The key is read from the `RITI_API_KEY` environment variable (`env_key`); nothing in these files
is secret. Notes from the runs:

- A turn with several parallel tool calls can fail on gateways that rewrite tool ids
  ("tool messages need a resolvable tool name"); keep such a model on single-call roles (planner).
- Some models accept only a subset of reasoning efforts (DeepSeek: low/high/xhigh/max): list
  exactly those in `efforts`, or the first turn is rejected with a 400.
- `fallback = "<alias>"` on a role lets Mantra switch model when the first one stays silent.
