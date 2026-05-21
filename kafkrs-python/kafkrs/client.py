"""Async TCP client for the kafkrs wire protocol v1.

Frame layout (network byte order):

    [total_size: u32][command_size: u32][Command protobuf][payload bytes]

total_size excludes itself; command_size is the length of the protobuf Command.
"""

from __future__ import annotations

import asyncio
import struct
from dataclasses import dataclass
from typing import List, Optional, Tuple

from kafkrs.wire import v1_pb2

PROTOCOL_VERSION = 1
MAX_FRAME_SIZE = 4 * 1024 * 1024


@dataclass
class FetchedRecord:
    offset: int
    timestamp_ns: int
    schema_id: int
    key: bytes
    value: bytes


class WireError(Exception):
    """Raised when the broker returns an ErrorResponse."""

    def __init__(self, code: int, message: str):
        self.code = code
        self.message = message
        super().__init__(f"wire error {code}: {message}")


class Client:
    """Single-connection async client. One Connect per Client.connect()."""

    def __init__(self, host: str, port: int, client_id: str = "kafkrs-python"):
        self._host = host
        self._port = port
        self._client_id = client_id
        self._reader: Optional[asyncio.StreamReader] = None
        self._writer: Optional[asyncio.StreamWriter] = None
        self._next_correlation_id = 1
        self._lock = asyncio.Lock()  # serialize on-socket I/O

    async def connect(self) -> None:
        self._reader, self._writer = await asyncio.open_connection(self._host, self._port)
        cmd = v1_pb2.Command()
        cid = self._next_id()
        cmd.correlation_id = cid
        cmd.connect.protocol_version = PROTOCOL_VERSION
        cmd.connect.client_id = self._client_id
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "connected":
            raise WireError(0, f"unexpected response to Connect: {resp.WhichOneof('body')}")
        if resp.connected.protocol_version != PROTOCOL_VERSION:
            raise WireError(
                0,
                f"broker protocol_version={resp.connected.protocol_version}; client={PROTOCOL_VERSION}",
            )

    async def close(self) -> None:
        if self._writer is not None:
            self._writer.close()
            try:
                await self._writer.wait_closed()
            except Exception:
                pass

    async def produce(
        self,
        topic: str,
        partition: int,
        records: List[Tuple[bytes, bytes]],
        schema_id: int = 0,
        timestamp_ns: int = 0,
    ) -> Tuple[int, int]:
        """Produce one or more (key, value) records. Returns (base_offset, last_offset)."""
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.produce.topic = topic
        cmd.produce.partition = partition
        payload = bytearray()
        for key, value in records:
            meta = cmd.produce.records.add()
            meta.key_len = len(key)
            meta.value_len = len(value)
            meta.schema_id = schema_id
            meta.timestamp_ns = timestamp_ns
            payload.extend(key)
            payload.extend(value)
        resp, _ = await self._roundtrip(cmd, bytes(payload))
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "produce_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        return resp.produce_resp.base_offset, resp.produce_resp.last_offset

    async def fetch(
        self,
        topic: str,
        partition: int,
        from_offset: int,
        max_records: int = 100,
        max_wait_ms: int = 0,
    ) -> Tuple[List[FetchedRecord], int]:
        """Fetch records starting at from_offset. Returns (records, hwm)."""
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.fetch.topic = topic
        cmd.fetch.partition = partition
        cmd.fetch.from_offset = from_offset
        cmd.fetch.max_records = max_records
        cmd.fetch.max_wait_ms = max_wait_ms
        resp, payload = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "fetch_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        out: List[FetchedRecord] = []
        cursor = 0
        for meta in resp.fetch_resp.records:
            kl = meta.key_len
            vl = meta.value_len
            key = bytes(payload[cursor : cursor + kl])
            value = bytes(payload[cursor + kl : cursor + kl + vl])
            cursor += kl + vl
            out.append(FetchedRecord(meta.offset, meta.timestamp_ns, meta.schema_id, key, value))
        return out, resp.fetch_resp.hwm

    async def create_topic(
        self,
        topic: str,
        partition_count: int,
        overrides: Optional[v1_pb2.TopicConfigOverrides] = None,
    ) -> None:
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.create_topic.topic = topic
        cmd.create_topic.partition_count = partition_count
        if overrides is not None:
            cmd.create_topic.overrides.CopyFrom(overrides)
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "create_topic_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")

    async def list_topics(self) -> List[str]:
        cmd = v1_pb2.Command()
        cmd.correlation_id = self._next_id()
        cmd.list_topics.SetInParent()
        resp, _ = await self._roundtrip(cmd, b"")
        if resp.WhichOneof("body") == "error":
            raise WireError(resp.error.code, resp.error.message)
        if resp.WhichOneof("body") != "list_topics_resp":
            raise WireError(0, f"unexpected response: {resp.WhichOneof('body')}")
        return list(resp.list_topics_resp.topics)

    # ---- Internals ----

    def _next_id(self) -> int:
        cid = self._next_correlation_id
        self._next_correlation_id += 1
        return cid

    async def _roundtrip(self, cmd: v1_pb2.Command, payload: bytes) -> Tuple[v1_pb2.Command, bytes]:
        async with self._lock:
            assert self._reader is not None and self._writer is not None
            cmd_bytes = cmd.SerializeToString()
            total_size = 4 + len(cmd_bytes) + len(payload)
            if 4 + total_size > MAX_FRAME_SIZE:
                raise WireError(0, "frame too large for client")
            self._writer.write(struct.pack(">II", total_size, len(cmd_bytes)))
            self._writer.write(cmd_bytes)
            if payload:
                self._writer.write(payload)
            await self._writer.drain()

            # Read response.
            outer = await self._reader.readexactly(4)
            (resp_total,) = struct.unpack(">I", outer)
            body = await self._reader.readexactly(resp_total)
            (resp_cmd_size,) = struct.unpack(">I", body[:4])
            resp_cmd = v1_pb2.Command()
            resp_cmd.ParseFromString(body[4 : 4 + resp_cmd_size])
            resp_payload = bytes(body[4 + resp_cmd_size :])
            return resp_cmd, resp_payload
