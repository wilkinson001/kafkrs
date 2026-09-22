# Broker Metrics — Design

**Status:** Draft for review
**Date:** 2026-09-21
**Scope:** A Prometheus scrape endpoint exposing 32 metrics across the broker's subsystems (produce, fetch, uploader, retention, partition state, runtime, wire errors). Naming follows OpenTelemetry semantic conventions where they exist. Every metric name and every label key is defined as a `pub const` in `kafkrs-server::metrics` so future renames are a one-edit change. Also bundled: a breaking restructure of the `ports` config into a typed `[ports]` section, driven by this feature.

## Motivation

kafkrs has zero observability today. The retention design (2026-09-21) explicitly flagged this as a gap — there is no way to know if retention is even running in production, no way to spot per-topic produce or fetch imbalances, no way to catch upload retries or DELETE failures. Every new feature increases the observability debt.

This spec closes the gap by adding a broker-wide metrics surface. The choice of Prometheus scrape (via the `metrics` crate façade) rather than native OTLP push is deliberate:

- **Call-site decoupling.** Code uses `metrics::counter!("name", "topic" => t)` with no knowledge of exporter. Swapping to OTLP later is a boot-time config change, not a rewrite.
- **Ops path of least resistance.** Prometheus scrape is what most operators run today, including OTel-native shops where the Collector's Prometheus receiver is the entry point.
- **OTel compatibility preserved.** The OpenTelemetry Collector natively scrapes Prometheus and translates to OTLP for any downstream backend. Names follow OTel `messaging.*` semantic conventions where applicable so backends auto-recognize the data.

Traces and logs are out of scope for this spec — a separate `tracing` + `tracing-opentelemetry` story lands when there's a driver.

## Design choices, with rationale

### Full broker surface in one release

v1 ships 32 metrics covering every subsystem. The alternative — instrument incrementally as subsystems get worked on — was rejected because it fragments the semantic-convention decision. Locking naming and cardinality once, up-front, means future features add metrics without renegotiating conventions.

### `metrics` façade + Prometheus scrape, not native OTLP

The `metrics` crate is the tokio-adjacent standard. Call sites are exporter-agnostic. `metrics-exporter-prometheus` provides the Prometheus scrape endpoint with classic histograms. `metrics-exporter-opentelemetry` is one config flip away when native OTLP push becomes worth it.

Rejected alternatives:
- **`opentelemetry` crate directly.** Heavier SDK, more API churn, and requires operators to run a Collector or accept OTLP directly. Prometheus scrape is the more universal starting point.
- **`tracing` + `tracing-opentelemetry` for metrics.** Metrics via tracing spans/events is contorted; call-site code becomes verbose. Better fit for future traces work.
- **`prometheus` crate directly.** Ties call sites to Prometheus permanently. The façade wins on flexibility.

### Dedicated admin port + typed ports config

Metrics are exposed as HTTP GET on a separate TCP port from the wire protocol. Rejected: protocol-detecting listener on the same port (peek first byte, dispatch to HTTP vs wire protocol). Doable but adds a protocol-sniffing layer to `accept_loop`, mixes concerns, and complicates future protocol evolution (TLS handshake vs plaintext HTTP).

The `ports` restructure is bundled deliberately. The existing `ports: Vec<u16>` shape has been flagged as over-engineered (why a list?) since it was written. Adding a second port type is the natural driver to restructure it now into a typed shape. Doing so as part of this feature avoids a follow-up "just rename ports" release.

Breaking config change accepted. Version bumps to 0.5.0 to signal it clearly.

### Per-topic labels always, per-partition behind opt-in flag

Cardinality is the load-bearing decision for a metrics rollout. Rejected alternatives:

- **Per-partition always.** At 10k partitions × 100 topics, `O(topics * partitions)` unique series is a real memory and scrape-time cost. Not sustainable at scale.
- **Per-topic only, ever.** Loses the ability to spot single-partition hot spots (e.g., "partition 47 has 100x the produce rate").
- **Runtime cardinality budget with silent drops.** User-hostile — metrics that vanish under load are worse than metrics with predictable cost.

The chosen policy matches Kafka's own convention: default is `O(topics)`, per-partition detail is opt-in via `broker.metrics_high_cardinality = true`. Broker-internal gauges (`kafkrs.runtime.uptime_seconds`, `kafkrs.wire.connections_active`) never carry topic or partition labels.

### Classic histograms, not native/sparse

Prometheus native (aka sparse or exponential) histograms are newer and better in most respects — automatic buckets, better tail resolution. They're rejected for v1 for compatibility: some scrapers, dashboards, and OTel Collector configurations still expect classic bucketed histograms. Adopting native histograms is a future release.

One shared bucket set for all latency histograms:

```rust
const LATENCY_BUCKETS_MS: &[f64] = &[
    0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0
];
```

Covers 0.5ms (in-memory fetch tail) to 5s (slow object-store upload). Standard roughly-exponential shape.

### Off by default

`ports.metrics` is an `Option<u16>`. When unset, `metrics::init` installs no recorder, binds no port, and returns. Every `counter!`/`histogram!` macro at call sites becomes essentially a nanoseconds-cost lookup + noop through the `metrics` crate's default recorder.

Rationale: upgrading to 0.5.0 must not silently bind a new port on a production host. Operators explicitly opt into observability.

## Architecture

One new module in kafkrs-server, one new dependency pair, plus in-place call-site instrumentation.

```
kafkrs-server/src/metrics.rs               ← NEW: setup fn + admin listener + shared label helpers
kafkrs-server/src/main.rs                  ← metrics::init call at boot
kafkrs-server/src/lib.rs                   ← pub mod metrics;
kafkrs-server/Cargo.toml                   ← + metrics = "0.24", metrics-exporter-prometheus = "0.16"

kafkrs-models/src/config.rs                ← Config.ports: PortsConfig; broker.metrics_high_cardinality

kafkrs-server/src/wire/dispatch.rs         ← produce + fetch counters, wire.rpc_requests
kafkrs-server/src/wire/mod.rs              ← wire.connections_{active,accepted} on accept_loop
kafkrs-server/src/fetcher.rs               ← fetch.source, fetch.long_poll_wait_ms
kafkrs-server/src/partition_writer.rs      ← partition.{records,bytes}_in_memory
kafkrs-server/src/uploader.rs              ← uploader.* + retention.{segments,bytes}_evicted, retention.passes, retention.delete_failures
kafkrs-server/src/retention_sweeper.rs     ← retention.sweep_kicks_{sent,dropped}
```

### `metrics.rs` responsibilities

```rust
static HIGH_CARDINALITY: AtomicBool = AtomicBool::new(false);
static START_TIME: OnceLock<Instant> = OnceLock::new();

pub fn init(ports: &PortsConfig, high_cardinality: bool) -> anyhow::Result<()> {
    let Some(port) = ports.metrics else { return Ok(()); };
    HIGH_CARDINALITY.store(high_cardinality, Ordering::Relaxed);
    START_TIME.set(Instant::now()).ok();

    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let builder = PrometheusBuilder::new()
        .set_buckets(LATENCY_BUCKETS_MS)?
        .with_http_listener(addr);
    builder.install()?;                       // installs global recorder + spawns HTTP listener
    describe_all();                           // help text + units for every metric name
    Ok(())
}

pub fn partition_label(topic: &str, partition: u32) -> Vec<(&'static str, String)> {
    let mut v = vec![("topic", topic.to_string())];
    if HIGH_CARDINALITY.load(Ordering::Relaxed) {
        v.push(("partition", partition.to_string()));
    }
    v
}

pub const LATENCY_BUCKETS_MS: &[f64] = &[
    0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 5000.0
];

fn describe_all() {
    metrics::describe_counter!("messaging.kafkrs.produce.records", "Records accepted by produce");
    metrics::describe_histogram!("messaging.kafkrs.produce.latency_ms", metrics::Unit::Milliseconds, "Server-side produce handling time");
    // ... 32 total describes ...
}
```

### Call-site pattern

Hot-path counter:
```rust
metrics::counter!("messaging.kafkrs.produce.records", "topic" => topic.clone())
    .increment(records.len() as u64);
```

Hot-path histogram:
```rust
let start = Instant::now();
// ... do work ...
metrics::histogram!("messaging.kafkrs.produce.latency_ms", "topic" => topic.clone())
    .record(start.elapsed().as_secs_f64() * 1000.0);
```

Per-partition-conditional labeling:
```rust
let labels = crate::metrics::partition_label(&topic, partition);
metrics::counter!("kafkrs.partition.records_in_memory", &labels).absolute(count);
```

### Boot sequence in `main.rs`

```rust
kafkrs_server::metrics::init(&cfg.ports, cfg.broker.metrics_high_cardinality)
    .expect("metrics init");
// ... partitions bringup ...
// ... registry spawn ...
// ... state construction ...
tokio::spawn(RetentionSweeper::new(partitions.clone(), sweep_interval).run());
for port in cfg.ports.wire.clone() {
    // ... existing wire accept_loop spawn ...
}
```

`metrics::init` is called first so the global recorder is installed before any subsystem starts emitting metrics.

### `wire.connections_*` in `accept_loop`

The `accept_loop` function in `kafkrs-server/src/wire/mod.rs` gains counter/gauge increments on TCP accept and disconnect. A wrapping guard struct decrements the active-connection gauge on Drop:

```rust
struct ConnectionGuard;
impl ConnectionGuard {
    fn new() -> Self {
        metrics::counter!("kafkrs.wire.connections_accepted").increment(1);
        metrics::gauge!("kafkrs.wire.connections_active").increment(1.0);
        ConnectionGuard
    }
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        metrics::gauge!("kafkrs.wire.connections_active").decrement(1.0);
    }
}
```

### `fetch.source` labeling

The Fetcher (`kafkrs-server/src/fetcher.rs`) already routes reads through three tiers: the in-memory active batch, the WAL file, and the object-store segment. The `messaging.kafkrs.fetch.source` counter increments once per successful fetch with the corresponding label value (`"memory"`, `"wal"`, or `"object_store"`), reflecting where the *majority* of the returned records came from. If a fetch spans multiple tiers, the counter increments once per tier's records.

### Non-instrumentation modules stay untouched

- `topic_registry.rs`, `recovery.rs`, `startup.rs`, `segment.rs`, `wal_writer.rs`, `object_store.rs` — these don't need instrumentation in v1. Registry operations are rare; recovery runs once at boot; the rest are called from already-instrumented paths and their metrics roll up through the caller.

## The metric list

Grouped by subsystem. Column key: **Type** — `C` counter, `G` gauge, `H` histogram. Names follow OpenTelemetry `messaging.*` semantic convention where consumers/producers are the actor; broker-internal state uses `kafkrs.*`.

### Wire path — produce

| Metric | Type | Labels | Description |
|---|---|---|---|
| `messaging.kafkrs.produce.records` | C | topic | Records accepted |
| `messaging.kafkrs.produce.bytes` | C | topic | Bytes accepted (value + key) |
| `messaging.kafkrs.produce.latency_ms` | H | topic | Server-side produce handling time |
| `messaging.kafkrs.produce.errors` | C | topic, error_code | Produce responses with an error |

### Wire path — fetch

| Metric | Type | Labels | Description |
|---|---|---|---|
| `messaging.kafkrs.fetch.requests` | C | topic | Fetch RPCs served |
| `messaging.kafkrs.fetch.records` | C | topic | Records returned |
| `messaging.kafkrs.fetch.bytes` | C | topic | Bytes returned |
| `messaging.kafkrs.fetch.latency_ms` | H | topic | Server-side fetch handling time (excludes long-poll wait) |
| `messaging.kafkrs.fetch.long_poll_wait_ms` | H | topic | Time spent waiting for tail data |
| `messaging.kafkrs.fetch.source` | C | topic, source (`memory`/`wal`/`object_store`) | Where the fetched data came from |
| `messaging.kafkrs.fetch.errors` | C | topic, error_code | Fetch errors (OffsetOutOfRange, UnknownTopic, etc.) |

### Uploader

| Metric | Type | Labels | Description |
|---|---|---|---|
| `kafkrs.uploader.segments_uploaded` | C | topic | Segments successfully uploaded to object store |
| `kafkrs.uploader.bytes_uploaded` | C | topic | Parquet bytes uploaded |
| `kafkrs.uploader.upload_latency_ms` | H | topic | Time from SealedBatch received to manifest updated |
| `kafkrs.uploader.upload_retries` | C | topic | Times an upload hit the retry loop |

### Retention

| Metric | Type | Labels | Description |
|---|---|---|---|
| `kafkrs.retention.segments_evicted` | C | topic | Segments deleted by retention |
| `kafkrs.retention.bytes_evicted` | C | topic | Sum of `byte_size` of evicted segments |
| `kafkrs.retention.passes` | C | topic, trigger (`upload`/`kick`) | Retention passes run |
| `kafkrs.retention.delete_failures` | C | topic | Object-store DELETEs that failed (accepted orphans) |
| `kafkrs.retention.sweep_kicks_sent` | C | — | Sweeper kicks enqueued (broker-wide) |
| `kafkrs.retention.sweep_kicks_dropped` | C | — | Kicks dropped due to full channel (`try_send` failures) |

### Partition state (gauges, updated on transitions)

| Metric | Type | Labels | Description |
|---|---|---|---|
| `kafkrs.partition.count` | G | — | Number of active partitions on the broker |
| `kafkrs.partition.records_in_memory` | G | topic | Active batch record count (sum across partitions in the topic when `metrics_high_cardinality = false`) |
| `kafkrs.partition.bytes_in_memory` | G | topic | Active batch byte size |
| `kafkrs.partition.segments_uploaded` | G | topic | Length of manifest.segments |
| `kafkrs.partition.hwm_offset` | G | topic | Highest committed offset per topic (max across partitions in the low-cardinality mode) |
| `kafkrs.partition.wal_files` | G | topic | WAL files present per topic (pre-upload) |

### Runtime + connections

| Metric | Type | Labels | Description |
|---|---|---|---|
| `kafkrs.runtime.uptime_seconds` | G | — | Broker process uptime |
| `kafkrs.runtime.build_info` | G | version, git_sha | Constant 1; label channel for version tracking |
| `kafkrs.wire.connections_active` | G | — | Currently open TCP connections |
| `kafkrs.wire.connections_accepted` | C | — | Cumulative connections accepted |
| `kafkrs.wire.rpc_requests` | C | rpc, error_code | RPCs served, by RPC kind + result code |

**Total: 32 metrics.**

**Per-partition variant.** When `broker.metrics_high_cardinality = true`, the following metrics additionally carry a `partition` label: all `messaging.kafkrs.produce.*`, `messaging.kafkrs.fetch.*`, `kafkrs.uploader.*`, `kafkrs.retention.{segments_evicted,bytes_evicted,passes,delete_failures}`, and all `kafkrs.partition.*` except `count`. The three broker-wide `kafkrs.retention.sweep_*` counters never carry a partition label. The `kafkrs.runtime.*` and `kafkrs.wire.*` metrics never carry topic or partition labels.

## Config surface

The top-level `ports: Vec<u16>` field is replaced by a typed `[ports]` section. `[broker]` gains one new field.

**Before (0.4.0):**
```toml
address = "127.0.0.1"
ports = [5432]
data_dir = "./data"
```

**After (0.5.0):**
```toml
address = "127.0.0.1"
data_dir = "./data"

[ports]
wire = [5432]
metrics = 9464      # optional; omit to disable /metrics endpoint

[broker]
disk_type = "nvme"
auto_create_topics = false
default_partition_count = 1
retention_sweep_interval_ms = 60000
metrics_high_cardinality = false
```

**Rust types:**

```rust
#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub address: String,
    pub data_dir: String,
    pub ports: PortsConfig,
    #[serde(default)]
    pub broker: BrokerConfig,
    pub object_store: ObjectStoreConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PortsConfig {
    pub wire: Vec<u16>,
    #[serde(default)]
    pub metrics: Option<u16>,
}

// BrokerConfig additions:
pub struct BrokerConfig {
    // ... existing 4 fields ...
    #[serde(default)]
    pub metrics_high_cardinality: bool,
}
```

**Migration.** Top-level `ports = [...]` no longer parses. Operators must migrate to `[ports]\nwire = [...]`. This is documented in the 0.5.0 changelog. Fresh config parse errors carry the field path (`ports`) so the failure mode is a clear TOML error, not a silent misconfiguration.

**Prometheus port choice.** No default value: `ports.metrics` is `Option<u16>`. When operators enable it, 9464 is the recommended value (OpenTelemetry Collector's Prometheus receiver default; not commonly conflicting on Linux). The `config.toml.example` shows `metrics = 9464` explicitly.

## Impact on existing code

| Location | Change |
| --- | --- |
| `kafkrs-models/src/config.rs` | Add `PortsConfig`; replace `Config.ports: Vec<u16>` with `Config.ports: PortsConfig`; add `BrokerConfig.metrics_high_cardinality: bool`. Update all unit tests. |
| `kafkrs-server/Cargo.toml` | Add `metrics = "0.24"` and `metrics-exporter-prometheus = "0.16"` (or current versions at implementation time). |
| `kafkrs-server/src/metrics.rs` | New file: `init`, `partition_label` helper, `LATENCY_BUCKETS_MS`, `describe_all`. |
| `kafkrs-server/src/lib.rs` | `pub mod metrics;`. |
| `kafkrs-server/src/main.rs` | Call `metrics::init` before spawning subsystems; loop over `cfg.ports.wire` (was `cfg.ports`). |
| `kafkrs-server/src/wire/dispatch.rs` | Instrument produce/fetch handlers; wire.rpc_requests counter per RPC. |
| `kafkrs-server/src/wire/mod.rs` | `ConnectionGuard` for wire.connections_{active,accepted}. |
| `kafkrs-server/src/fetcher.rs` | fetch.source (per tier), long_poll_wait_ms. |
| `kafkrs-server/src/partition_writer.rs` | Update `partition.records_in_memory`/`bytes_in_memory` gauges on active-batch transitions; `hwm_offset` on commit. |
| `kafkrs-server/src/uploader.rs` | `uploader.*` counters/histogram; `retention.{segments,bytes}_evicted`, `retention.passes`, `retention.delete_failures`. |
| `kafkrs-server/src/retention_sweeper.rs` | `retention.sweep_kicks_{sent,dropped}` counters. |
| `kafkrs-server/tests/wire_e2e.rs` | New e2e test: enable `ports.metrics = 0`, produce/fetch, scrape `/metrics`, assert series present. Update every fixture that constructs `Config`/`PortsConfig`. |
| `config.toml.example` | Rewrite for new schema. |
| `README.md` | Update quickstart example. |

No breaking changes to the wire protocol, no changes to `TopicConfigOverrides`, no changes to `PartitionHandle` or `SharedState`, no dependencies added to other crates.

## Versioning

Bump all three crates from 0.4.0 to **0.5.0** in lockstep.

- **Breaking config schema change** (top-level `ports` moves under `[ports]`) — sufficient on its own for a minor version.
- **New bound network port** (opt-in but adds a surface) — deserves a signal to operators.
- **Wire protocol version stays at 1.** No proto changes.

The kafkrs-python crate bumps for lockstep. No Python API changes; existing scripts continue to work.

## Test plan

### Unit tests

**`kafkrs-models/src/config.rs`:**
- `new_ports_shape_parses` — verify TOML with `[ports]\nwire = [5432]\nmetrics = 9464` parses correctly.
- `metrics_port_optional` — verify `[ports]\nwire = [5432]` (no `metrics`) parses with `metrics = None`.
- `metrics_high_cardinality_default_false` — verify absent `metrics_high_cardinality` resolves to `false`.
- `metrics_high_cardinality_parses_when_set` — verify `metrics_high_cardinality = true` parses.
- `old_ports_shape_fails_helpfully` — verify top-level `ports = [5432]` returns a TOML error mentioning `ports` field.

**`kafkrs-server/src/metrics.rs`:**
- `partition_label_returns_topic_only_when_disabled`.
- `partition_label_returns_both_when_enabled`.
- `init_with_no_metrics_port_is_noop` — `metrics::init(&PortsConfig { wire: vec![5432], metrics: None }, false)` returns Ok and no listener binds.
- `latency_buckets_are_monotonically_increasing`.

### Integration tests

**`kafkrs-server/tests/wire_e2e.rs`:** new test `metrics_endpoint_exposes_produce_counters`:
1. Spin up broker with `ports.metrics = 0` (auto-bind), produce 3 records to topic `t`.
2. Scrape the metrics endpoint via a raw HTTP `GET /metrics` on the bound port.
3. Assert response body contains `messaging_kafkrs_produce_records{topic="t"} 3` (Prometheus text format converts dots to underscores in the exposition, but the internal name stays with dots).
4. Assert response body contains `kafkrs_wire_connections_accepted` gauge.

**`kafkrs-server/tests/wire_e2e.rs`:** new test `metrics_high_cardinality_toggle_adds_partition_label`:
- Same shape, once with `metrics_high_cardinality = false` (assert no `partition="..."` in exposition for produce metrics), once with `true` (assert `partition="0"` present).

**`kafkrs-server/tests/wire_e2e.rs`:** update every `setup_broker*` fixture to construct the new `PortsConfig` shape. Fixtures that don't need metrics can pass `metrics: None`.

### Manual smoke

Not needed if the integration tests scrape and assert. If desired, a smoke test runs the broker with `ports.metrics = 9464`, curls `/metrics`, and eyeballs the output.

## Out of scope

### Deferred to future work
- **Native OTLP push exporter.** Swap the Prometheus exporter for `metrics-exporter-opentelemetry`. Config gains a `[metrics]` section for endpoint URL, protocol, TLS. Zero call-site changes.
- **Traces.** `tracing` + `tracing-opentelemetry` alongside metrics. Different call-site API; separate spec.
- **`/health` endpoint.** Trivial to add on the metrics admin port once a driver appears (Kubernetes probes, load balancer health checks).
- **`/debug/*` endpoints** (goroutine dumps, allocator stats, etc.). Same admin port. Not needed at current scale.
- **Native/sparse Prometheus histograms.** Better tail resolution; adopt when scrapers and dashboards catch up.
- **Metric-server TLS.** Requires a broader broker-TLS story.
- **Per-consumer-group metrics.** Requires consumer groups to exist first.
- **Dynamic cardinality budget.** If operators want a hard cap, add `max_series` config to the exporter builder. Currently the boolean flag is sufficient.

### Not in scope at all
- **Multi-broker metrics coordination.** Each broker exposes its own scrape endpoint; Prometheus (or Collector) aggregates. No cross-broker roll-up in the broker itself.
- **Custom metrics for downstream users.** The wire protocol doesn't expose metrics-producing hooks. Users needing custom metrics run their own instrumentation stack.

## Invariants (for implementers)

1. **The global recorder is installed exactly once, at boot.** After `metrics::init`, no other code calls `set_global_recorder`.
2. **Every hot-path metric emits `topic` as a label.** No hot-path metric emits `partition` unless `metrics_high_cardinality = true` is set at boot.
3. **When `ports.metrics` is `None`, `set_global_recorder` is not called and no port is bound.** Call sites remain but become no-ops via the `metrics` crate's default recorder.
4. **Metric names are stable within a major version.** Once a metric ships with a name, it does not change until the next major release. Semantic conventions locked in v1 do not silently rename. A project-wide rename (e.g., `kafkrs.*` and `messaging.kafkrs.*` prefixes changing) is a valid reason for a major version bump; individual metric names shifting between minor versions is not.
5. **No `.await` on the recorder path.** All `metrics::` macros are synchronous and lock-free.
6. **Latency histograms use `LATENCY_BUCKETS_MS`.** No per-metric bucket customization in v1.
7. **`HIGH_CARDINALITY` is set once at `metrics::init` and read (never written) from call sites via `partition_label`.** Runtime changes to cardinality policy require broker restart.
