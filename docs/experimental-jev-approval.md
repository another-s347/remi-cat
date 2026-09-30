# Experimental Jev tool approval

This feature changes only the model review of tool calls that have no known risk rule. Known tool rules still classify calls without Jev. It does not moderate chat messages.

The feature is off by default. Add this to the selected profile's `runtime.yaml`:

```yaml
experimental_jev_approval:
  enabled: true
  profile: jev
  min_low_probability_bps: 9900 # P(low) >= 0.99
```

On first use, Remi Cat seeds `decision-models/jev.yaml` in the profile data directory. The built-in template uses TypeSafe's `https://api.typesafe.ai/v1` and `jev-latest`. You may edit `base_url`, `model`, `api_key_env`, and `timeout_ms`, or create another `<profile>.yaml` in that directory and reference its ID. Set `REMI_DECISION_MODELS_DIR` if the profiles live elsewhere. The URL is the API base; Remi Cat appends `/systemone`. Only the TypeSafe System One request and response protocol is supported.

Set the key named by `api_key_env` in the existing Secret Store or process environment. The default is `TYPESAFE_API_KEY`. Never put the key in YAML. The runtime validates the selected profile and key when the feature is enabled.

Jev receives the existing redacted tool argument summary as `state` and answers one `choice` question with `low`, `medium`, or `high`. Only `low` with `P(low) >= min_low_probability_bps / 10000` can auto-run under the low-risk session policy. Every other result, including a response below the threshold or a service failure, enters human approval. The approval view shows the selected level, `P(low)`, and returned model version when available.

This is an experimental classifier, not a guarantee that an unknown tool is safe. Validate the threshold using representative tool calls before enabling automatic execution.
