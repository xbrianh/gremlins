"""SchemeResolver protocol."""

from __future__ import annotations

from typing import Any, Protocol, runtime_checkable

from gremlins.artifacts.uri import Uri


@runtime_checkable
class SchemeResolver(Protocol):
    def read(self, uri: Uri) -> Any: ...
    def verify_produced(self, uri: Uri) -> None: ...
