"""The one check every dev script runs on a server address before it connects.

Soak runs, sweeps and the proxy create farms, move fences and flood reports.
Pointed at staging or production they would do that to a real deployment,
so they drive this machine only. Pure string work: no DNS, no socket.

    from local_only import local_only, local_hostport
    api = farm.Api(local_only(url))           # http(s)/ws(s)://127.0.0.1|localhost|[::1]:port
    host, port = local_hostport("127.0.0.1:17150")

scripts/local-only.mjs is the same check for bun and node scripts, and
scripts/check-local-only.py fails CI for a script that connects without one.
"""

from urllib.parse import urlsplit

LOCAL_HOSTS = {"127.0.0.1", "localhost", "::1"}


class NotLocal(ValueError):
    pass


def local_only(url: str) -> str:
    """`url` without a trailing slash, or NotLocal when it isn't a server on this machine."""
    try:
        u = urlsplit(url)
        u.port
    except ValueError as e:
        raise NotLocal(f"{url!r} is not a URL ({e})")
    if u.scheme not in ("http", "ws"):
        raise NotLocal(f"{url!r}: dev scripts drive plain http/ws on this machine, never {u.scheme or 'a bare host'}")
    if u.hostname not in LOCAL_HOSTS:
        raise NotLocal(f"{url!r}: {u.hostname} is not this machine. Dev scripts never drive staging or production; run a local server (CONTRIBUTING.md)")
    return url.rstrip("/")


def local_hostport(hostport: str) -> tuple:
    """`host:port` for a raw TCP upstream, checked the same way."""
    host, _, port = hostport.rpartition(":")
    local_only(f"http://{host}:{port}")
    return host.strip("[]"), int(port)
