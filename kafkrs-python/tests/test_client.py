"""End-to-end test: spawn a real kafkrs-server, connect with the Python
client, exercise the full Connect → CreateTopic → Produce → Fetch loop."""

import asyncio
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from kafkrs import Client
from kafkrs.client import WireError
from kafkrs.wire import v1_pb2


REPO_ROOT = Path(__file__).resolve().parents[2]
BROKER_BIN = REPO_ROOT / "target" / "debug" / "kafkrs-server"


def _find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _build_broker_if_needed() -> None:
    if BROKER_BIN.exists():
        return
    subprocess.run(
        ["cargo", "build", "--bin", "kafkrs-server"],
        cwd=str(REPO_ROOT),
        check=True,
    )


def _write_config(tmp: Path, port: int) -> Path:
    cfg = tmp / "config.toml"
    data_dir = tmp / "data"
    data_dir.mkdir()
    cfg.write_text(
        f"""
address = "127.0.0.1"
data_dir = "{data_dir.as_posix()}"

[ports]
wire = [{port}]

[broker]
disk_type = "nvme"
auto_create_topics = true
default_partition_count = 1
cluster_id = "test-cluster"

[object_store]
backend = "filesystem"
bucket = "test"
prefix = ""
endpoint = ""
region = "us-east-1"
"""
    )
    return cfg


def _wait_for_port(host: str, port: int, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.1)
    raise RuntimeError(f"broker did not start listening on {host}:{port}")


@pytest.fixture
def broker():
    _build_broker_if_needed()
    port = _find_free_port()
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        cfg_path = _write_config(tmp, port)
        env = dict(os.environ)
        env["RUST_LOG"] = "warn"
        proc = subprocess.Popen(
            [str(BROKER_BIN), str(cfg_path)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            _wait_for_port("127.0.0.1", port)
            yield port
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


@pytest.mark.asyncio
async def test_connect_produce_fetch_roundtrip(broker: int) -> None:
    client = Client("127.0.0.1", broker)
    await client.connect()
    try:
        # auto_create_topics=true in the config, so producing creates the topic.
        base, last = await client.produce(
            "demo",
            0,
            [(b"k1", b"v1"), (b"", b"v2")],
        )
        assert base == 0
        assert last == 1

        recs, hwm = await client.fetch("demo", 0, from_offset=0, max_records=10, max_wait_ms=500)
        assert len(recs) == 2
        assert recs[0].offset == 0 and recs[0].key == b"k1" and recs[0].value == b"v1"
        assert recs[1].offset == 1 and recs[1].key == b"" and recs[1].value == b"v2"
        assert hwm >= 1
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_unsupported_version_raises(broker: int) -> None:
    # Monkey-patch the constant for this test only.
    import kafkrs.client as mod
    saved = mod.PROTOCOL_VERSION
    mod.PROTOCOL_VERSION = 999
    try:
        client = Client("127.0.0.1", broker)
        with pytest.raises(WireError) as ei:
            await client.connect()
        assert ei.value.code == 100  # ERR_UNSUPPORTED_PROTOCOL_VERSION
    finally:
        mod.PROTOCOL_VERSION = saved


def _write_config_no_auto_create(tmp: Path, port: int) -> Path:
    cfg = tmp / "config.toml"
    data_dir = tmp / "data"
    data_dir.mkdir()
    cfg.write_text(
        f"""
address = "127.0.0.1"
data_dir = "{data_dir.as_posix()}"

[ports]
wire = [{port}]

[broker]
disk_type = "nvme"
auto_create_topics = false
default_partition_count = 1
cluster_id = "test-cluster"

[object_store]
backend = "filesystem"
bucket = "test"
prefix = ""
endpoint = ""
region = "us-east-1"
"""
    )
    return cfg


@pytest.fixture
def broker_no_auto_create():
    _build_broker_if_needed()
    port = _find_free_port()
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        cfg_path = _write_config_no_auto_create(tmp, port)
        env = dict(os.environ)
        env["RUST_LOG"] = "warn"
        proc = subprocess.Popen(
            [str(BROKER_BIN), str(cfg_path)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            _wait_for_port("127.0.0.1", port)
            yield port
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()


@pytest.mark.asyncio
async def test_create_topic_then_produce(broker_no_auto_create: int) -> None:
    client = Client("127.0.0.1", broker_no_auto_create)
    await client.connect()
    try:
        await client.create_topic("explicit", partition_count=1)
        base, last = await client.produce("explicit", 0, [(b"k", b"v")])
        assert base == 0
        assert last == 0

        recs, _hwm = await client.fetch("explicit", 0, from_offset=0, max_wait_ms=200)
        assert len(recs) == 1
        assert recs[0].key == b"k"
        assert recs[0].value == b"v"
    finally:
        await client.close()


@pytest.mark.asyncio
async def test_delete_topic_removes_data(broker_no_auto_create: int) -> None:
    # auto_create_topics=false so a produce after delete raises ErrUnknownTopic
    # instead of silently recreating the topic.
    async with Client("127.0.0.1", broker_no_auto_create) as c:
        await c.create_topic("smoke-delete", partition_count=1)
        await c.produce("smoke-delete", 0, [(b"k", b"v")])
        await c.delete_topic("smoke-delete", delete_data=True)

        # Subsequent produce should raise WireError with ErrUnknownTopic.
        with pytest.raises(WireError) as exc_info:
            await c.produce("smoke-delete", 0, [(b"k", b"v")])
        # ErrUnknownTopic is code 200 per v1.proto.
        assert exc_info.value.code == 200


@pytest.mark.asyncio
async def test_alter_topic_config_round_trip(broker_no_auto_create: int) -> None:
    async with Client("127.0.0.1", broker_no_auto_create) as client:
        topic = "alter-config-round-trip"
        await client.create_topic(topic, partition_count=1)

        overrides = v1_pb2.TopicConfigOverrides()
        overrides.retention_ms = 60_000
        resp = await client.alter_topic_config(topic, overrides)
        assert resp.retention_ms == 60_000


@pytest.mark.asyncio
async def test_alter_topic_config_unknown_raises(broker_no_auto_create: int) -> None:
    async with Client("127.0.0.1", broker_no_auto_create) as client:
        overrides = v1_pb2.TopicConfigOverrides()
        with pytest.raises(WireError) as excinfo:
            await client.alter_topic_config("does-not-exist", overrides)
        assert excinfo.value.code == v1_pb2.ERR_UNKNOWN_TOPIC


@pytest.mark.asyncio
async def test_alter_topic_config_invalid_value_raises(broker_no_auto_create: int) -> None:
    async with Client("127.0.0.1", broker_no_auto_create) as client:
        topic = "alter-config-invalid"
        await client.create_topic(topic, partition_count=1)

        overrides = v1_pb2.TopicConfigOverrides()
        overrides.segment_size_bytes = 0
        with pytest.raises(WireError) as excinfo:
            await client.alter_topic_config(topic, overrides)
        assert excinfo.value.code == v1_pb2.ERR_INVALID_CONFIG


async def test_get_metadata_returns_broker_and_topic_info(broker_no_auto_create):
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        await client.create_topic("orders", partition_count=2)
        resp = await client.get_metadata()
        assert resp.cluster_id  # non-empty
        assert len(resp.brokers) == 1
        assert resp.brokers[0].broker_id  # non-empty
        names = [t.topic for t in resp.topics]
        assert "orders" in names
        t = next(t for t in resp.topics if t.topic == "orders")
        assert t.error_code == 0
        assert t.topic_uuid  # non-empty
        assert len(t.partitions) == 2
        for p in t.partitions:
            assert p.leader_broker_id == resp.brokers[0].broker_id


async def test_get_metadata_filter_returns_subset(broker_no_auto_create):
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        await client.create_topic("a", partition_count=1)
        await client.create_topic("b", partition_count=1)
        resp = await client.get_metadata(topics=["a"])
        names = [t.topic for t in resp.topics]
        assert names == ["a"]


async def test_get_metadata_unknown_topic_has_per_topic_error(broker_no_auto_create):
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        await client.create_topic("real", partition_count=1)
        resp = await client.get_metadata(topics=["real", "not-a-topic"])
        by_name = {t.topic: t for t in resp.topics}
        assert by_name["real"].error_code == 0
        assert by_name["real"].topic_uuid  # non-empty
        assert len(by_name["real"].partitions) == 1
        assert by_name["not-a-topic"].error_code == v1_pb2.ERR_UNKNOWN_TOPIC
        assert by_name["not-a-topic"].topic_uuid == ""
        assert len(by_name["not-a-topic"].partitions) == 0


async def test_connect_populates_cluster_id_on_response(broker_no_auto_create):
    # The Connect response is consumed by Client.connect(), which doesn't
    # currently expose the parsed ConnectedResponse. This test drives out
    # a client-side change to make cluster_id observable, OR runs a raw
    # Connect through _roundtrip to inspect the response directly.
    # Follow the second approach — it exercises the wire without changing
    # the Client public API.
    port = broker_no_auto_create
    async with Client("127.0.0.1", port) as client:
        # Send a Ping/Pong to prove the client is connected, then inspect
        # nothing further — the meaningful assertion is that the wire
        # request/response roundtrip already validated cluster_id shape.
        # For an assertion, do a raw Connect + response inspection via the
        # module-level helper. Since the Client already consumed the
        # single Connect on construction, dial a fresh socket:
        pass

    # Fresh raw socket to inspect the Connect response.
    import asyncio, struct
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    try:
        cmd = v1_pb2.Command()
        cmd.correlation_id = 42
        cmd.connect.protocol_version = 1
        cmd.connect.client_id = "cluster-id-test"
        cmd_bytes = cmd.SerializeToString()
        total_size = 4 + len(cmd_bytes)
        writer.write(struct.pack(">II", total_size, len(cmd_bytes)))
        writer.write(cmd_bytes)
        await writer.drain()

        outer = await reader.readexactly(4)
        (resp_total,) = struct.unpack(">I", outer)
        body = await reader.readexactly(resp_total)
        (resp_cmd_size,) = struct.unpack(">I", body[:4])
        resp = v1_pb2.Command()
        resp.ParseFromString(body[4 : 4 + resp_cmd_size])
        assert resp.WhichOneof("body") == "connected"
        assert resp.connected.broker_id  # non-empty
        assert resp.connected.cluster_id  # non-empty
    finally:
        writer.close()
        await writer.wait_closed()
