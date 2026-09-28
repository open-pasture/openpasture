"""The §6 test farm around Ames and small helpers for scripts/soak.sh (dev tool).

Coordinates are local metres from the farm centre [-93.62, 42.03]: x east,
y north. P1 is x -413..0, y 0..400; P2 east of it; P3 north of P1.
"""

import json
import math
import time
import urllib.error
import urllib.request

CENTER = (-93.62, 42.03)
R = 6371008.8


def to_lonlat(x, y):
    lat = CENTER[1] + math.degrees(y / R)
    lon = CENTER[0] + math.degrees(x / (R * math.cos(math.radians(CENTER[1]))))
    return [round(lon, 7), round(lat, 7)]


def to_xy(p):
    x = math.radians(p[0] - CENTER[0]) * R * math.cos(math.radians(CENTER[1]))
    y = math.radians(p[1] - CENTER[1]) * R
    return x, y


def rect(x0, y0, x1, y1):
    ring = [to_lonlat(x0, y0), to_lonlat(x1, y0), to_lonlat(x1, y1), to_lonlat(x0, y1)]
    return {"type": "Polygon", "coordinates": [ring + [ring[0]]]}


# The §6 paddocks, as their corners in degrees.
P1 = {"type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]]}
P2 = {"type": "Polygon", "coordinates": [[[-93.62, 42.03], [-93.615, 42.03], [-93.615, 42.0336], [-93.62, 42.0336], [-93.62, 42.03]]]}
P3 = {"type": "Polygon", "coordinates": [[[-93.625, 42.0336], [-93.62, 42.0336], [-93.62, 42.0372], [-93.625, 42.0372], [-93.625, 42.0336]]]}
P1_W, P1_E = to_xy([-93.625, 42.03])[0], 0.0
P1_N = to_xy([-93.62, 42.0336])[1]
P3_S, P3_N = P1_N, to_xy([-93.62, 42.0372])[1]


class Api:
    def __init__(self, base, token=None):
        self.base = base.rstrip("/")
        self.token = token

    def call(self, method, path, body=None, ok=(200, 201, 204)):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method)
        req.add_header("content-type", "application/json")
        if self.token:
            req.add_header("authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                raw = r.read()
                status = r.status
        except urllib.error.HTTPError as e:
            raw, status = e.read(), e.code
        if status not in ok:
            raise RuntimeError(f"{method} {path}: {status} {raw[:300]!r}")
        return json.loads(raw) if raw else None

    def get(self, path):
        return self.call("GET", path)

    def post(self, path, body=None):
        return self.call("POST", path, body if body is not None else {})

    def put(self, path, body):
        return self.call("PUT", path, body)


def wait_up(api, timeout=120):
    t = time.time()
    while time.time() - t < timeout:
        try:
            api.get("/api/server")
            return
        except Exception:
            time.sleep(0.5)
    raise RuntimeError("server did not come up")


def setup(api, herds, public_url=None):
    """The §6 farm with P1-P3 and `herds` = [(name, count, paddock)]. Returns {name: id}, {paddock: id}."""
    api.post("/api/farm", {"name": "Test farm", "timezone": "America/Chicago", "center": list(CENTER)})
    if public_url:
        api.put("/api/settings", {"server": {"public_url": public_url}})
    pads = {}
    for name, g in (("P1", P1), ("P2", P2), ("P3", P3)):
        pads[name] = api.post("/api/paddocks", {"name": name, "geometry": g})["id"]
    ids = {}
    for name, count, pad in herds:
        ids[name] = api.post("/api/herds", {"name": name, "species": "cattle", "count": count, "paddock_id": pads[pad]})["id"]
    return ids, pads


def positions(api, herd_id):
    """Fresh positions of a herd's collars, local metres, by collar id."""
    out = {}
    for p in api.get(f"/api/positions?herd_id={herd_id}") or []:
        fix = p.get("fix") or {}
        if fix.get("point"):
            out[p["collar_id"]] = to_xy(fix["point"])
    return out


def status(api, herd_id):
    return api.get(f"/api/herds/{herd_id}/boundary")
