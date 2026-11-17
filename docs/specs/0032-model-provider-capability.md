# 0032 — Model Provider Capability

## Problem

Every launcher that configures models today gets one model at a time through
`agent-model`, or one per sub-agent. A user who configured several models had
to add one capability instance per model to make them selectable in the tool.
The tool's own model picker stayed blind to everything else in
`granite-cli`'s configuration.

## Shape

A new `BindingType::ModelProvider` and a new capability type
`model-provider`. The capability has no settings of its own: it covers every
configured model.

`ModelProviderCapability::resolve_refs` resolves every configured model
through `ModelLookup::configured_ids`. Its metadata declares
`Dependency::Models`, the whole collection rather than one config key, so:

- the validator walks every model the capability would expose, and a model
  whose provider is gone is reported before anything binds;
- `model_ids_named_by` in the proxy route registration sees every configured
  model, so each one gets a route keyed by the name the launched process will
  send.

`bind` takes a `ModelProviderBindingRequest` with the API dialects the
launcher can express, in preference order. It produces one entry per
configured provider, named after the provider instance:

- a provider is exposed under the first requested dialect it serves with a
  `Chat` endpoint, so its models appear once and the entry says which
  granite-cli provider they came from;
- a model that does not support `Chat` is left out;
- a provider that serves none of the requested dialects is left out;
- each entry carries the chosen dialect's endpoint path, and its models'
  names, context lengths and reasoning efforts.

The result is a `ModelProviderBinding` with one `ModelProviderEntry` per
non-empty provider. Entry names are the provider instance ids (e.g.
`my-ollama`), so two providers that speak the same dialect stay distinct.

## Proxy

The entry's URL is the session proxy's, not a real provider's. The launcher
sets it at launch time from `LaunchContext::model_proxy`; a dry run has no
proxy, so it prints `http://127.0.0.1:<session-proxy>` and the version prefix.
The proxy dispatches each request by its top-level `"model"` field to the
route the launch registered for that model, and records usage there. Because
a model name is not necessarily unique across providers, the entry's own
name identifies the granite-cli provider the user chose; the proxy still
routes by the model name the request carries.

## Launchers

`opencode` and `pi` implement the binding first:

- OpenCode: one `provider.<name>` entry per provider, `npm` set to
  `@ai-sdk/openai-compatible` or `@ai-sdk/anthropic`, `options.baseURL` at
  the proxy. No top-level `model` is written, so the user picks.
- Pi: one `providers.<name>` entry per provider, `api` set to
  `openai-completions` or `anthropic-messages`, `baseUrl` at the proxy, and
  the placeholder API key Pi needs to make the models selectable. No
  `--provider`/`--model` is passed, so the user picks. A model entry omits
  `contextWindow` when the model reports no context length (0), because
  Pi's schema rejects a non-positive value and then ignores every provider
  in the file; Pi's own default applies instead.

Both launchers still select the bound model when `agent-model` is also
enabled; the two capabilities can coexist in one generated config. When they
name the same provider, the model lists are merged and the `agent-model`
entry's options are kept, as long as both chose the same dialect.

Other launchers are left for a follow-up. Their metadata does not list
`ModelProvider`, so enabling the capability there is rejected as an
unsupported binding.

## Not solved

- Provider custom headers are still not injected by the proxy; a provider
  that needs headers the launched client does not send can fail through a
  route. This is a pre-existing limitation of proxied routes.
- No shipped provider type serves only Anthropic, so a provider that offers
  *only* Anthropic is covered by unit tests with a fake provider. In the
  fixture, Ollama serves both dialects and is exposed under OpenAI, so the
  Anthropic branch is not exercised end to end.
