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
ports = [{port}]
data_dir = "{data_dir.as_posix()}"

[broker]
disk_type = "nvme"
auto_create_topics = true
default_partition_count = 1

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
