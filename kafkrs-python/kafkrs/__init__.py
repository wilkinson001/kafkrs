"""Pure-Python client for the kafkrs broker."""

__version__ = "0.3.1"
__all__ = ["Client"]


def __getattr__(name: str):
    if name == "Client":
        from kafkrs.client import Client  # noqa: PLC0415
        return Client
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
