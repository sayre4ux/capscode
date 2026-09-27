# Authenticating

You bring your own credentials. CapsCode reads them from the host, keeps them in the gateway,
and hands the tool a sentinel. A credential is persisted only when you explicitly use
`capscode key set <provider>` or choose credential storage during interactive `capscode init`;
provider defaults selected during setup are stored separately and never contain secrets.

**API key** (any provider):

```bash
export ANTHROPIC_API_KEY=sk-ant-...      # or OPENAI_API_KEY / MOONSHOT_API_KEY
capscode claude
```

**Codex subscription login:** CapsCode automatically reads a valid ChatGPT login from Codex's
existing `$CODEX_HOME/auth.json` (or `~/.codex/auth.json`). Run `codex login` once on the host,
then verify discovery with:

```bash
capscode key ls
capscode codex
```

`CODEX_ACCESS_TOKEN` is an explicit one-run override. `OPENAI_API_KEY` remains supported for
API-key usage. CapsCode reads the existing Codex authentication file but does not copy it into
the capsule; only a sentinel reaches the tool, while the real credential stays in the gateway.
It asks you to refresh a ChatGPT token that is expired or expires within one hour, and it also
supports Codex API-key logins created by `codex login --with-api-key`.

**Claude subscription (Pro/Max) via a long-lived OAuth token** from `claude setup-token`:

```bash
claude setup-token                       # produces sk-ant-oat01-...
export CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-...
capscode claude
```

**OS keychain (recommended)** — store the token once in your keychain and CapsCode reads it
at run time, so it never sits in your shell environment:

```bash
# macOS
security add-generic-password -a "$USER" -s "claude-setup-token" -w "sk-ant-oat01-..."
# Linux (GNOME Keyring / KWallet via Secret Service)
secret-tool store --label="claude setup token" service claude-setup-token

capscode config set auth.keychain true
capscode key ls     # lists stored keys and credential kinds, never values
capscode claude          # no env var needed
```

CapsCode reads the **existing** `claude-setup-token` item (no new naming). You can also let
CapsCode manage it: `capscode key set anthropic` (stores to the keychain), `capscode key rm anthropic`.
Keychain lookups are **opt-in**: storing a credential through CapsCode enables them. For an
existing item, enable lookups with `capscode config set auth.keychain true`; until then CapsCode
never touches the keychain, so you never get a surprise authorization dialog.

**Every other provider — the provider keystore.** Non-Anthropic API keys (OpenAI, Kimi, …)
can be persisted the same way instead of living in your shell profile:

```bash
capscode key set kimi-coding    # hidden prompt; or pipe the key via stdin
capscode pi --provider kimi-coding       # no export needed from now on

capscode key ls                 # ids and dates only, never values
capscode key rm kimi-coding
capscode key reset              # delete the whole bundle (confirmed)
```

All provider keys share **one** keychain item, so the OS asks for access at most once per
binary. `key set` enables keychain lookups automatically. The keys stay host-side: at
launch the selected provider's key goes to the gateway over tmpfs, and the capsule still
sees only a sentinel.

**Credential precedence:**

- Anthropic: `CLAUDE_CODE_OAUTH_TOKEN` → `ANTHROPIC_API_KEY` → OS keychain (opt-in).
- OpenAI/Codex: `CODEX_ACCESS_TOKEN` → `OPENAI_API_KEY` → Codex `auth.json`.
- Everything else: its `key_env` environment variable → provider keystore (opt-in).

An explicit environment variable is always a one-run override. A missing or locked keychain is
non-fatal for resolution, and an unreadable keystore fails a launch with its own diagnosis
instead of pretending the key is missing. `capscode key ls` lists stored credentials without
printing their values.

**Kimi:** the *coding-plan* subscription uses the bundled `kimi-coding` /
`kimi-coding-openai` providers (`KIMI_API_KEY`, host `api.kimi.com/coding`); the Moonshot
*platform* API uses `--provider kimi` / `kimi-openai` (China: `kimi-cn` / `kimi-cn-openai`)
with `MOONSHOT_API_KEY`. Both speak
Anthropic's or OpenAI's API shape depending on the variant, so they run through the
matching tools with a provider override.

**Z.AI / GLM:** the bundled registry splits Z.AI by product (standard API vs Coding Plan),
region (`.ai` vs `.cn`), and wire protocol. International entries use `GLM_API_KEY`; China
entries use `GLM_CN_API_KEY`. The Anthropic-style ids are `zai-api-anthropic`,
`zai-api-cn-anthropic`, `zai-coding`, and `zai-coding-cn`; the standard API ids `zai-api` and
`zai-api-cn` are OpenAI-style, as are `zai-coding-openai` and `zai-coding-cn-openai`. Use the
Anthropic-style ids with `claude`, and OpenAI-style ids with `codex`, `opencode`, `pi`, or
`omp`. `capscode provider list` shows the bundled providers.

**OpenRouter:** the bundled registry pins `moonshotai/kimi-k2.6` as the default model, but
OpenRouter is an aggregator whose catalog changes constantly, so listing it statically would
be wrong within a week. At launch CapsCode fetches `https://openrouter.ai/api/v1/models`
**from the host** — before the capsule exists — and folds the returned ids into the config it
generates for catalog-driven tools (`capscode omp`, `pi`, `opencode`), with the default model
kept first and duplicates removed. The credential goes to `curl` through a config file on
stdin, never argv or an environment variable, so it stays out of `ps`, the capsule, the
generated config, and every error message. Discovery is best-effort: if the request fails or
returns something unreadable, CapsCode says so and launches on the registry's default model.
Every other provider is served entirely from the static registry.
