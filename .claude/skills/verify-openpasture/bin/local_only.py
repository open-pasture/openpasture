"""The one check every verify-openpasture entry point runs before it opens a socket.

A base URL is driven only if it is plain http on this machine (127.0.0.1,
localhost or [::1]) and not port 7878, where a real farm runs. Anything else
(a staging or production host, a LAN address, https, a name that needs DNS)
is refused before any lookup or connection is made.

    python3 local_only.py <url>     exit 0 and print the URL, or exit 3 and say why
"""

import sys
from urllib.parse import urlsplit

LOCAL_HOSTS = {"127.0.0.1", "localhost", "::1"}
REAL_FARM_PORT = 7878


class NotLocal(Exception):
    pass


def local_only(url: str) -> str:
    """Return `url` without a trailing slash, or raise NotLocal. Pure string work: no DNS, no socket."""
    try:
        u = urlsplit(url)
        port = u.port
    except ValueError as e:
        raise NotLocal(f"{url!r} is not a URL ({e})")
    if u.scheme != "http":
        raise NotLocal(f"{url!r}: only http on this machine is driven, not {u.scheme or 'no scheme'}")
    if u.hostname not in LOCAL_HOSTS:
        raise NotLocal(f"{url!r}: {u.hostname} is not this machine. verify-openpasture never drives staging, production or another host")
    if port is None:
        raise NotLocal(f"{url!r}: name the port of the stack you launched")
    if port == REAL_FARM_PORT:
        raise NotLocal(f"{url!r}: port {REAL_FARM_PORT} is where a real farm runs; launch a verify stack instead")
    if u.username or u.password or u.query or u.fragment or u.path not in ("", "/"):
        raise NotLocal(f"{url!r}: give the bare base URL, like http://127.0.0.1:17990")
    return url.rstrip("/")


if __name__ == "__main__":
    try:
        print(local_only(sys.argv[1] if len(sys.argv) > 1 else ""))
    except NotLocal as e:
        print(f"refused: {e}", file=sys.stderr)
        sys.exit(3)
