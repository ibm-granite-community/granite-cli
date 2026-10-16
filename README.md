# Granite CLI

🚧 🚧 🚧 UNDER CONSTRUCTION 🚧 🚧 🚧

A command-line tool for discovering, configuring, and launching Granite-powered local and remote AI workflows.

## Quick Start

### Install with the one-liner script

```sh
curl -fsSL https://raw.githubusercontent.com/IBM-granite-community/granite-cli/main/install.sh | bash
```

This script automatically:
- Downloads the latest prebuilt binary (when available for your platform)
- Falls back to `cargo install` if no binary exists
- Builds from source as a last resort

**Environment variables:**

| Variable | Purpose |
|---|---|
| `GRANITE_CLI_VERSION` | Release tag to install (default: latest release) |
| `GRANITE_CLI_INSTALL_DIR` | Custom installation directory |
| `VERBOSE` | Set to `1` for verbose output |
| `CI` or `NONINTERACTIVE` | Set to `1` for non-interactive / CI mode |
| `NO_SETUP` | Set to a truthy value (`1`, `t`, `y`, `true`, `yes`) to skip `granite-cli setup` after install |
| `AUTO` | Set to a truthy value to run setup in auto/non-interactive mode (default: `false`; enabled automatically in CI) |
| `PULL` | Set to a truthy value to pull models automatically during setup (default: `false`) |
| `NO_COMPLETIONS` | Set to a truthy value to skip installing the shell completion script (see [Shell completion](#shell-completion)) |

### Install from source

```sh
git clone https://github.com/IBM-granite-community/granite-cli.git
cd granite-cli
cargo install --path .
```

The binary will be placed in `~/.cargo/bin` (or `$CARGO_INSTALL_ROOT/bin` if set).

### Install on Termux

```sh
curl -fsSL https://raw.githubusercontent.com/IBM-granite-community/granite-cli/main/install.sh | bash
```

On Termux the script will:
- Use `~/bin` as the default install directory (already on PATH)
- Detect the Termux environment and automatically install required packages (`openssl`, `pkg-config`, `clang`, `perl`, `binutils`)
- Configure the build to use Termux's system OpenSSL and gnu17 C standard

If building from source is required, the script handles all Termux-specific build configuration automatically.

### Verify installation

```sh
granite-cli --help
```

### Shell completion

`granite-cli completions <SHELL>` prints a completion script to stdout. Supported shells are `bash`, `zsh`, `fish`, `elvish` and `powershell`. The install script writes the script for bash and fish to the directories below, and prints the zsh steps.

**bash** (requires the `bash-completion` package for the directory-based setup):

```sh
mkdir -p ~/.local/share/bash-completion/completions
granite-cli completions bash > ~/.local/share/bash-completion/completions/granite-cli
```

Or load it from `~/.bashrc`:

```sh
source <(granite-cli completions bash)
```

The bash 3.2 that ships with macOS does not support `source <(...)`. Use `eval "$(granite-cli completions bash)"` there.

**zsh**: write `_granite-cli` into a directory on `fpath`:

```sh
mkdir -p ~/.zfunc
granite-cli completions zsh > ~/.zfunc/_granite-cli
```

Then add these lines to `~/.zshrc`, before any existing `compinit` call:

```sh
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```

**fish**:

```sh
mkdir -p ~/.config/fish/completions
granite-cli completions fish > ~/.config/fish/completions/granite-cli.fish
```

Regenerate the script after upgrading `granite-cli` so that new commands and flags complete.

## Roadmap

Today, the project already supports:
- browsing a catalog of Granite models
- configuring models and providers
- checking provider health
- recommending models based on local hardware
- pulling model weights for supported local providers
- rendering output in the terminal, as plain text, JSON, and Markdown

The longer-term goal is for Granite CLI to become the control plane for Granite-based developer tooling: a single place to manage models, providers, capabilities, and launcher integrations for tools like coding assistants and agentic workflows.

### Current direction

The project is currently focused on building a strong foundation in a few areas:

- **Model management**: catalog, search, recommendation, setup, and pull flows for Granite models
- **Provider management**: support for local and OpenAI-compatible providers, including health checks and model compatibility
- **Capability architecture**: describing higher-level Granite capabilities that can be attached to tools and workflows
- **Launcher architecture**: eventually launching external tools with Granite-aware configuration and overlays
- **Multiple output modes**: terminal-first UX with machine-readable output formats for scripting and automation

### What the project wants to be

Granite CLI is aiming to become:

1. a **developer-facing interface** for working with Granite models and capabilities
2. a **local setup and orchestration tool** for providers like Ollama, LM Studio, llama.cpp, and compatible servers
3. a **bridge layer** between Granite model/capability definitions and downstream tools such as assistants, launchers, and IDE workflows
4. a **repeatable automation surface** that works well both interactively and in scripts/CI

### Near-term themes

- improving CI and contributor workflows
- expanding provider support
- expanding model metadata and registry coverage
- building out TUI setup and browsing flows
- implementing capability and launcher architecture
- improving documentation and release automation

This repository is still under active construction. The architecture is taking shape, but some areas are intentionally incomplete while core foundations are being built.

## Capability Support

The table below shows which capabilities each launcher supports.
It is generated from the `supported_capabilities` declared in each launcher's source file.
To regenerate it after adding a launcher or capability, run `scripts/update-capability-table.sh`.

<!-- capability-table-start -->

| Capability | Type | `claude` | `bob` | `pi` | `opencode` | `hermes` | `goose` | `openclaw` | `codex` |
|---|---|:---:|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| `agent-model` | Agent Model Binding | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `vision-mcp` | Vision MCP Server | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `sub-agent` | Sub-Agent | ✅ | ✅ | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ |
| `sub-agent-code` | Code Sub-Agent | ✅ | ✅ | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ |
| `sub-agent-explore` | Explore Sub-Agent | ✅ | ✅ | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ |
| `sub-agent-plan` | Plan Sub-Agent | ✅ | ✅ | ❌ | ✅ | ❌ | ❌ | ❌ | ✅ |

**Capability descriptions:**

| Capability | Description |
|---|---|
| `agent-model` | Surfaces a configured model's connection details (base URL, model name, auth, TLS) to a launched agent. |
| `vision-mcp` | Exposes a vision-language model as an MCP server (compare/analyze images) for a launched coding agent to call. |
| `sub-agent` | Defines a named sub-agent (prompt, tool allow-list, and model) that a launched coding agent can delegate to. |
| `sub-agent-code` | Defines a named coding sub-agent (static prompt, fixed tools, and model) that a launched coding agent can delegate to. |
| `sub-agent-explore` | Defines a named exploration sub-agent (static prompt, fixed tools, and model) that a launched coding agent can delegate to. |
| `sub-agent-plan` | Defines a named planning sub-agent (static prompt, fixed read-only tools, and model) that a launched coding agent can delegate implementation-plan design to. |

**Binding types** used to determine compatibility:
- **AgentModel** — passes a model's connection details directly to the launcher
- **Mcp** — exposes the capability as an MCP server the launcher can call
- **SubAgent** — exposes the capability as a named sub-agent the launcher can delegate to

<!-- capability-table-end -->

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for full details. We follow a fork-and-PR workflow with [DCO](https://developercertificate.org/) sign-off, and ask that all participants follow our [Code of Conduct](CODE_OF_CONDUCT.md).
