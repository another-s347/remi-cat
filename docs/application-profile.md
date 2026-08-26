# Application Profile

An Application Profile is a project-level, cross-session assembly manifest for a runnable agent application. It describes which configuration and resources a process should use; it is not a security boundary and does not own the referenced resources.

The runtime resolves the manifest into `InstanceProfile`. Code embedding remi-cat can use the exported `ResolvedApplicationProfile` alias to make the distinction explicit, and can use the exported `ProfileRegistry` / `RegisteredProfile` API for the same registration and discovery semantics as the CLI.

## Manifest

The default filename is `profile.yaml`. Relative paths are resolved from the directory containing the manifest.

```yaml
schema_version: 1
id: travel.planner
name: Travel Planner
description: Plans and maintains project travel
version: 1.0.0
workspace: ./project

config:
  runtime: ./config/runtime.yaml
  channels: ./config/channels.yaml

resources:
  agents: ./agents
  models: ./models
  skills:
    - ./skills
    - ../shared-skills
  workflows: ./workflows

state:
  data: ./state
  sessions: ./state/sessions.json
  memory: ./state/memory
  users: ./state/users.json
  tasks: ./state/tool_tasks

capabilities:
  tags: [travel]
  intents: [plan-trip]
  channels: [tui, web, feishu, a2a]

endpoint:
  type: local
  command: remi-cat --profile "${PROFILE}" a2a stdio
```

`config`, `resources`, and `state` contain references, not embedded definitions. A referenced path may be shared by multiple profiles. Secrets and credentials continue to use the existing process-level stores and are intentionally not part of this manifest.

`config.channels` references the concrete channel instances assembled for this profile. It is deliberately separate from `capabilities.channels`: capabilities are discovery metadata, while this file controls which connectors actually run.

```yaml
channels:
  - kind: feishu
    id: work
    enabled: true
    transport: web_socket
    credentials:
      app_id_env: FEISHU_WORK_APP_ID
      app_secret_env: FEISHU_WORK_APP_SECRET

  - kind: feishu
    id: travel
    enabled: true
    transport: event_hook
    event_hook:
      host: 127.0.0.1
      port: 9788
      path: /feishu/travel
      verification_token_env: FEISHU_TRAVEL_VERIFICATION_TOKEN
    credentials:
      app_id_env: FEISHU_TRAVEL_APP_ID
      app_secret_env: FEISHU_TRAVEL_APP_SECRET
```

Credential fields are key references resolved through the existing secret store and process environment. Multiple connector instances run concurrently. Their session, user, file, and sub-session routing namespaces are independent; the built-in default remains `feishu`, while named instances use `feishu:<instance-id>`.

`endpoint` is a strict tagged union. A local endpoint is a one-line command that serves A2A over framed stdin/stdout. The process is started on demand for a conversation; it does not have to be serving beforehand. These placeholders are expanded before launch:

- `${PROFILE}`: absolute manifest path, or the profile name for a legacy named profile
- `${PROFILE_ID}`: stable manifest ID
- `${PROFILE_DIR}`: manifest directory
- `${INSTANCE}`: selected runtime instance name
- `${WORKSPACE}`: resolved workspace, when configured

Because `endpoint.command` is executed by the platform shell, profile manifests must be treated as executable configuration.

The remote shape is reserved in schema version 1, but invocation is intentionally not implemented yet:

```yaml
endpoint:
  type: remote
  url: https://agent.example.com/a2a
  auth:
    type: bearer
    token_env: TRAVEL_AGENT_TOKEN
```

## Selection and compatibility

Profile references have one unambiguous form for each source:

- `default`: built-in compatibility profile
- `@travel`: registered alias
- `id:travel.planner`: registered stable ID (must resolve uniquely)
- `./travel/profile.yaml`: manifest path
- `legacy-name`: existing profile beneath the legacy data-root

Use a manifest directly:

```sh
remi-cat --profile ./profile.yaml tui
remi-cat profile show ./profile.yaml
remi-cat --profile ./profile.yaml a2a stdio
```

Named profiles remain supported. `remi-cat profile create NAME` now writes both `runtime.yaml` and `profile.yaml`.

When no profile is selected, remi-cat synthesizes the built-in `remi.default` profile. Its paths preserve the previous defaults: `runtime.yaml`, `agents`, `models`, `skills`, `workflows`, `sessions.json`, `memory`, `users.json`, and `tool_tasks` beneath the existing data directory.

## Progressive management workflow

Start with discovery and inspection. These commands do not mutate profile resources:

```sh
remi-cat profile current
remi-cat profile list
remi-cat profile find --tag travel --channel web
remi-cat profile show @travel
remi-cat profile show @travel --manifest
remi-cat profile show @travel --resolved
remi-cat profile show @travel --sources
remi-cat profile check @travel --strict
remi-cat profile resource list @travel
remi-cat profile resource show @travel config.channels
```

Create and register a manifest explicitly:

```sh
remi-cat profile init ./travel --id travel.planner --name "Travel Planner"
remi-cat profile register ./travel/profile.yaml --alias travel
remi-cat profile set @travel capabilities.channels web,feishu
remi-cat profile set @travel endpoint.local.command 'travel-agent --a2a-stdio'
remi-cat profile unset @travel description
remi-cat profile unregister @travel
```

Configure concrete IM connector instances in the profile's referenced
`channels.yaml`. These commands store environment-variable names, not secret
values; `capabilities.channels` remains discovery metadata rather than runtime
configuration:

```sh
remi-cat profile channel list @travel
remi-cat profile channel upsert-feishu @travel work \
  --transport websocket \
  --app-id-env TRAVEL_FEISHU_APP_ID \
  --app-secret-env TRAVEL_FEISHU_APP_SECRET
remi-cat profile channel disable @travel work
remi-cat profile channel enable @travel work
remi-cat profile channel remove @travel work --force
```

Run a profile as a persistent Web/IM-serving process. Named instances allow
multiple independently controlled processes for the same profile:

```sh
remi-cat profile start @travel --instance work
remi-cat profile status @travel --instance work
remi-cat profile restart @travel --instance work
remi-cat profile stop @travel --instance work
remi-cat profile status --all --format json
```

Instance records and logs live under
`<registry-root>/profile-instances/<profile-id>/`. A channel configuration
change takes effect after restarting the corresponding managed instance.

`register` and `unregister` only update the registry. They never copy or delete a manifest, resource, state file, or credential. Registry writes use a cross-process lock and atomic file replacement. Use `profile registry info`, `list`, and `repair` to inspect it.

The registry is process-global by default and lives beneath the user's remi-cat home directory (`~/.remi-cat`), independently of the current working directory and the selected profile's state directory. Set `REMI_PROFILE_REGISTRY_ROOT` only when an embedding host or isolated test needs an explicit registry location.

For explicitly authorized non-interactive self-management, local prompt and
CLI entrypoints accept `--permissions medium`. This auto-approves low- and
medium-risk tools for that session while high-risk operations remain blocked:

```sh
remi-cat prompt --permissions medium \
  "Create and validate a travel profile, then configure its Feishu channel"
```

## Local discovery and conversation

No process registry, lease, heartbeat, or endpoint control plane is required for local profiles. The caller resolves a registered manifest, starts its command, verifies its A2A Agent Card, sends a task, and stops the child when the request completes:

```sh
remi-cat profile ask @travel "plan a five-day trip to Kyoto"
remi-cat profile ask @travel --named summer-2027 "refine the hotel shortlist"
```

Every remi-cat runtime also registers two model-facing tools:

- `external_agent_discover`: finds registered profiles by declared tags and intents.
- `external_agent_ask`: starts a local target on demand and talks to it through A2A.

Registering a local profile is the execution trust boundary. Both tools are low-risk from the agent approval system's perspective so non-interactive agents can delegate directly; only registered manifests are discoverable or addressable through these tools.

The caller passes a stable named conversation key so repeated A-to-B requests map to the same A2A context and the target's cross-session state. A child receives `REMI_PROFILE_*` identity, resolved resource/state paths, `REMI_PROFILE_REGISTRY_ROOT`, and the same process-level secret backend or absolute dotenv source, so it can discover and call another local profile in turn without introducing profile-scoped credentials.

The stdio binding uses a four-byte big-endian payload length followed by an A2A JSON message. Agent Card, streaming message, cancellation, and shutdown operations are represented explicitly. Direct `endpoint.type: remote` manifests remain protocol reservations and currently return `REMOTE_AGENT_NOT_IMPLEMENTED`; remotely discovered Profile Hub references use the authenticated Weaver transport described below.

## Embedded application API

`ApplicationBuilder::profile(&manifest)` attaches a stable profile descriptor to an embedded application. `ApplicationBuilder::external_agents(registry_root)` installs the same `external_agent_discover` and `external_agent_ask` tools used by the CLI runtime, with the attached profile ID as the caller identity. `ApplicationHandle::profile()` exposes its identity, description, version, declared capabilities, and endpoint; `ApplicationHandle::catalog()` separately exposes the resolved model, agent, skill, and workflow catalog.

## Profile Hub discovery

Remi Cat can consume multiple named Profile Hub directories concurrently. Hub
replication, registration, leases, device identity, and topology are owned
entirely by the remote service; the runtime only performs authenticated,
read-only discovery. Configure the selected application's `runtime.yaml`:

```yaml
profile_hubs:
  - id: office
    enabled: true
    url: http://office-profile-hub.virtual
    weaver_network: office
    token_env: OFFICE_PROFILE_HUB_TOKEN
    request_timeout_ms: 5000
  - id: home
    enabled: true
    url: http://home-profile-hub.virtual
    weaver_network: home
    token_env: HOME_PROFILE_HUB_TOKEN
    request_timeout_ms: 5000
```

Each Hub references one already-provisioned Weaver membership. Multiple Hubs
may share a membership and its single live `NetworkHandle`; Hubs on different
Weaver networks remain cryptographically isolated:

```yaml
weaver_networks:
  - id: office
    data_dir: weaver/office
    master_key_file: secrets/office.member-key
    root_public_key: <64 lowercase hex characters>
    app_addr: <client AppAddr from the signed application binding>
    device_id: <DeviceId from the signed client binding>
    relay_only: false
```

Relative paths are resolved from `runtime.yaml`. Provision the membership and
application binding with `weaver-cli prepare-join`, `join`, `app-prepare`, and
`app-bind`; Remi Cat only opens the resulting membership and never creates or
modifies the virtual network.

Set `relay_only: true` for diagnostics or deployments that must disable direct
IP/LAN candidates and force all Weaver traffic through the signed relay.

Token values stay in the existing environment or secret store. Each URL is a
logical Hub entrypoint, regardless of whether that service is standalone,
primary, or a manually configured replica. IDs must be unique within the
runtime and URLs must be absolute, port-free `http://*.virtual` origins without
paths. Names resolve only through the selected Weaver network's signed Virtual
DNS zone and are never sent to system DNS. Connections
are queried concurrently; a failed Hub does not suppress results from healthy
Hubs or the local registry, while failure of every configured Hub is reported.

`external_agent_discover` merges matching local registry entries with online
Hub profiles. Remote results use opaque
`hub:<hub-id>/<hub_profile_id>` references and include their Hub ID, device ID,
and `source: profile_hub`. Passing one of these references to
`external_agent_ask` or `remi-cat profile ask` fetches its Agent Card through
the selected Hub and proxies the streaming A2A conversation through the same
authenticated Weaver network. Direct remote manifest endpoints remain
unsupported and never receive fallback local execution.

`serve_application_a2a_stdio(handle)` serves an embedded application through the framed stdio binding. Both HTTP and stdio A2A Agent Cards are generated from the handle descriptor, so embedded and CLI-started applications publish the same profile identity.
