# kafkrs roadmap

Living document — updated as priorities shift and features land. Structured as the ordered route to multi-broker, since that's the largest single architectural undertaking on the horizon and most other work is either part of the path or defensibly parallel to it.

## Where we are today

Single-broker only. As of **0.6.0**:

- **Storage:** WAL + fsync-gated producer acks; async Parquet segment uploads to object store; per-partition manifest.
- **Wire protocol v1:** Connect, Produce, Fetch, CreateTopic, DescribeTopic, ListTopics, DeleteTopic. Reserved ranges for streaming-consumer RPCs (40-49) and remaining admin RPCs (52-59).
- **Retention (0.4.0):** time + size, per-topic overrides, Uploader-embedded + broker-wide sweeper.
- **Metrics (0.5.0):** Prometheus scrape endpoint on a dedicated admin port; 37 metrics; OTel `messaging.*` semconv where applicable; per-partition labels behind a config flag.
- **DeleteTopic (0.6.0):** mark-and-sweep with fast client response; per-delete ephemeral cleanup task; restart-safe pending state; UUIDv7 baked into every topic's object-store prefix so `Delete + Create` under the same name is race-free.

## Phase 1 — Shore up single-broker ergonomics

Small, high-value work that's easier to do before cluster complexity lands. Each ships value on its own.

### 1.1 Broker `/health` endpoint

Orchestrators (K8s, Nomad, load balancers) need to know when a broker is alive vs stuck. Once multi-broker lands, cluster-membership monitoring uses `/health` too. Tiny scope — the admin port already exists from metrics work.

### 1.2 `AlterTopicConfig`

Retention, metrics, and DeleteTopic have all entrenched the "topic config is immutable after CreateTopic" pattern. Changing `retention_ms` or `metrics_high_cardinality` today requires a broker restart. In multi-broker this becomes a distributed problem (config change must propagate to replicas); doing it single-broker first forces us to design the "config change → running actor" propagation pattern once, then extend it. Doing it after multi-broker would mean designing config propagation and cluster consistency at the same time — much harder.

## Phase 2 — Wire protocol lock-in

Do these BEFORE multi-broker so the protocol doesn't churn during that work.

### 2.1 `Metadata` RPC

Clients need to ask "which broker owns partition P of topic T?". In single-broker: trivial ("always me"). In multi-broker: essential. Landing the RPC now means Python (and future) clients can start caching metadata, wire protocol stays additive, and multi-broker doesn't need to also invent a client-facing discovery protocol.

### 2.2 Broker identity + `cluster_id`

Add `broker_id: string` and `cluster_id: string` to `ConnectedResponse`. In single-broker: both static from config. In multi-broker: clients detect they're talking to the right cluster and route by broker_id. Three-line proto change + one config field.

### 2.3 Consumer groups + offset commit

The largest single missing feature vs Kafka. Enables stateful consumers with resumable progress, load balancing across a group, rebalance on membership change. Real new use cases (event-driven services, streaming pipelines, changelog processing) unlock.

Placement here is deliberate. The argument for landing consumer groups BEFORE multi-broker: land the wire surface (Fetch with group semantics, `OffsetCommit` / `OffsetFetch` / `JoinGroup` / `Heartbeat` RPCs) with a "coordinator = this broker" placeholder, so the API is stable. Multi-broker then redesigns just the coordination internals, not the client-facing API. Doing it after would mean designing the coordinator and the client API simultaneously — that's where Kafka itself got a lot of things wrong we can avoid.

Consumer groups are also a huge chunk of kafkrs's real-world usefulness. Landing them unblocks users who don't care about multi-broker.

## Phase 3 — Coordination substrate

This is where multi-broker really starts. Pick a coordination model before anything else in this phase.

### 3.1 Consensus / coordination decision

Three viable models:

- **Embedded Raft** (via `openraft` or similar) — brokers form a Raft group, one elected controller. Highest complexity, no external deps.
- **External etcd / consul** — brokers use etcd for leader election + shared state. Simplest to implement, adds an ops dependency.
- **Controller-broker pattern** (Kafka's old ZK-less variant) — one broker manually designated controller, others heartbeat to it. Simplest architecturally, least fault-tolerant.

This is a genuine brainstorm — the choice cascades into every subsequent decision. Do it as its own spec + design doc before writing any cluster code.

### 3.2 Distributed topic registry

Once 3.1 lands, `topics.json` becomes cluster state instead of per-broker. Topic list, UUIDs, config, partition assignments all move to whichever store the coordination layer uses. `AlterTopicConfig`'s propagation design (Phase 1.2) pays off here.

### 3.3 Broker cluster membership

Config format for broker discovery (static peers list vs DNS SRV vs cloud discovery), heartbeat protocol, membership changes propagating through the coordination layer.

## Phase 4 — Partition ownership + failover

### 4.1 Partition leadership assignment

The controller (or Raft-elected leader) decides which broker owns writes for each partition. Includes assignment algorithm (round-robin, weighted, sticky), reassignment on broker join/leave/fail, and `Metadata` RPC (Phase 2.1) starts returning real assignments instead of "always me."

### 4.2 Broker failover mechanics

kafkrs has a large head start here: the object store is already shared and durable. When broker A fails and broker B takes over its partitions, B doesn't need to catch up from a replication log — it reads the manifest and continues.

But: broker A's in-flight WAL data (fsync'd but not uploaded) needs to be either recovered by B or acknowledged as lost. Design options:

- **(a)** WAL lives on shared storage too (network FS, NVMe-over-fabric).
- **(b)** Accept the loss with a documented durability boundary (producer ack downgrades to "durable in-broker" instead of "durable cluster-wide").
- **(c)** Synchronous WAL replication to another broker.

Massive design decision. Whichever we pick shapes the producer-ack story for the entire multi-broker era.

### 4.3 Consumer group coordinator election

Now that leadership exists, the group coordinator becomes a partition-leadership problem (Kafka assigns each group to a specific broker via consistent hashing). Consumer groups from Phase 2.3 gain real multi-broker coordination behind the same wire API.

## Phase 5 — Client updates

### 5.1 Python client multi-broker routing

Cache metadata, route to the correct broker per partition, retry on `NOT_LEADER` / `LEADER_NOT_AVAILABLE`, invalidate cache on errors, connect to any bootstrap broker then discover the rest.

Future clients in other languages inherit the design from here.

## Phase 6 — Multi-broker

### 6.1 Multi-broker mode

By this point the coordination layer, metadata protocol, leadership assignment, failover mechanics, and client-side routing all exist. Turning on multi-broker becomes primarily an integration + testing exercise rather than a design one.

## Critical path to multi-broker

Strictly blocking items — everything else in Phases 1-5 is defensibly "do before multi-broker to save pain later" rather than strictly required:

- **2.1** `Metadata` RPC
- **2.2** Broker identity
- **3.1** Consensus decision
- **3.2** Distributed topic registry
- **3.3** Cluster membership
- **4.1** Partition leadership
- **4.2** Failover mechanics
- **5.1** Client routing

## Honest recommendation

Don't rush to multi-broker. Single-broker with `AlterTopicConfig` + consumer groups + `/health` + distributed tracing is a genuinely useful product on its own. Multi-broker is a multi-quarter effort; picking up the ergonomics wins first means shipping capability along the way while the v2 architecture gets designed properly.

## Parallel tracks (not blocking multi-broker)

Work that's valuable but doesn't gate the multi-broker path — sequence around user demand:

- **Distributed tracing** (`tracing` + `tracing-opentelemetry`). Natural round-out after metrics. Enables per-request tail-latency debugging. Especially valuable during multi-broker development for cross-broker request flows.
- **Native OTLP push exporter.** Metrics spec's deferred hook. One-line exporter swap once someone wants to point kafkrs at an OTel Collector directly instead of Prometheus scrape.
- **Compaction** (`cleanup.policy=compact`). Rounds out the storage-management family alongside retention. Different algorithm from retention (key-based dedup, requires reading + rewriting Parquet). No known user driver yet.
- **Auth / TLS.** No driver yet; naturally piggybacks on future admin-port TLS work.
- **Housekeeping sprint** for the minor follow-ups from prior reviews: `RegistryMsg::Delete` returning `(uuid, partition_count)` to close the Describe→Delete TOCTOU; `describe_reply` distinguishing `ErrBrokerNotReady` from `ErrUnknownTopic` on channel drop; shutdown ack timeout logging instead of discarding; removing the unused `PartitionWriter.topic_uuid` field. Half a day total.
