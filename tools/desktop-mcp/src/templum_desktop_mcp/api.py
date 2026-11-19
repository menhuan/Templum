"""The small, authenticated part of Templum's HTTP API this bridge needs."""

from __future__ import annotations

import ipaddress
import json
import os
import re
import stat
from contextlib import asynccontextmanager
from pathlib import Path
from urllib.parse import urlsplit

import httpx


class BridgeError(Exception):
    def __init__(self, code: str, message: str, **details):
        self.result = {"error": {"code": code, "message": message, **details}}
        super().__init__(message)


def read_token(path: Path) -> str:
    """Only read an ordinary, user-owned file with no group or other permissions."""
    try:
        fd = os.open(path.expanduser(), os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        with os.fdopen(fd) as source:
            info = os.fstat(source.fileno())
            if not stat.S_ISREG(info.st_mode):
                raise BridgeError("config", "The token file must be an ordinary file.")
            if os.name == "posix" and (info.st_uid != os.getuid() or info.st_mode & 0o077):
                raise BridgeError("config", "The token file must belong to you; use chmod 600 on it.")
            token = source.read(256).strip()
    except (OSError, UnicodeError):
        raise BridgeError("config", "Cannot read the token file; create a user-only token file first.") from None
    if not re.fullmatch(r"tpl_[A-Za-z0-9_-]{43}", token):
        raise BridgeError("config", "The token file must contain only the Templum API token.")
    return token


def local_url(value: str) -> str:
    try:
        parts = urlsplit(value)
        host = ipaddress.ip_address(parts.hostname or "")
        port = parts.port
        valid = (
            host.is_loopback
            and parts.scheme in {"http", "https"}
            and parts.username is None
            and parts.password is None
            and parts.path in {"", "/"}
            and not parts.query
            and not parts.fragment
            and port != 0
        )
    except ValueError:
        valid = False
    if not valid:
        raise BridgeError("config", "Use an http(s) loopback IP URL, such as http://127.0.0.1:7411; forward SSH to it.")
    return value.rstrip("/")


class Api:
    def __init__(self, url: str, token: str, *, transport=None):
        self._token = token
        self.http = httpx.AsyncClient(
            base_url=local_url(url),
            headers={"Authorization": f"Bearer {token}"},
            timeout=45,
            follow_redirects=False,
            trust_env=False,
            transport=transport,
        )

    def redact(self, value):
        return json.loads(json.dumps(value, ensure_ascii=False).replace(self._token, "[redacted]"))

    async def close(self):
        await self.http.aclose()

    async def check(self, response: httpx.Response):
        if response.status_code == 200:
            return
        try:
            error = json.loads(await response.aread())["error"]
            code, message = error["code"], error["message"]
        except (ValueError, KeyError, TypeError):
            code, message = "http", "Templum did not return an API error. Check its address and API settings."
        raise BridgeError(code, message, http_status=response.status_code)

    async def request(self, method: str, path: str, body=None):
        try:
            response = await self.http.request(method, path, json=body) if body is not None else await self.http.request(method, path)
            await self.check(response)
            return response.json()
        except httpx.TimeoutException:
            raise BridgeError("timeout", "Templum did not answer in time. A write may have happened; inspect the canvas before retrying.") from None
        except httpx.HTTPError:
            raise BridgeError("connection", "Cannot reach Templum. Check its API and, if used, the SSH forward.") from None
        except ValueError:
            raise BridgeError("protocol", "Templum returned invalid JSON.") from None

    async def tool(self, name: str, arguments: dict):
        return await self.request("POST", f"/v1/tools/{name}", arguments)

    @asynccontextmanager
    async def events(self, cursor: str | None = None):
        headers = {"Last-Event-ID": cursor} if cursor else {}
        try:
            async with self.http.stream("GET", "/v1/events", headers=headers) as response:
                await self.check(response)
                if not response.headers.get("content-type", "").startswith("text/event-stream"):
                    raise BridgeError("protocol", "Templum did not return its event stream.")
                yield self._events(response)
        except httpx.TimeoutException:
            raise BridgeError("timeout", "Templum's event stream did not answer in time.") from None
        except httpx.HTTPError:
            raise BridgeError("connection", "Templum's event stream disconnected. Retry with the same cursor.") from None

    async def _events(self, response):
        event, identifier, data = "message", None, []
        size = 0
        async for line in response.aiter_lines():
            size += len(line)
            if size > 16 * 1024 * 1024:
                raise BridgeError("protocol", "Templum's event exceeded 16 MiB.")
            if not line:
                if data:
                    try:
                        yield {"event": event, "id": identifier, "data": json.loads("\n".join(data))}
                    except ValueError:
                        raise BridgeError("protocol", "Templum sent an invalid event.") from None
                event, identifier, data, size = "message", None, [], 0
            elif not line.startswith(":"):
                key, _, value = line.partition(":")
                value = value.removeprefix(" ")
                if key == "event":
                    event = value
                elif key == "id":
                    identifier = value
                elif key == "data":
                    data.append(value)
