"""scripts/local_only.py: the soak and dev scripts drive this machine only.

    python3 -m unittest discover -s scripts -p 'test_*.py'
"""

import os
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from local_only import NotLocal, local_hostport, local_only  # noqa: E402

STAGING = "https://op-server-staging-staging.up.railway.app"


class LocalOnly(unittest.TestCase):
    def test_this_machine_passes(self):
        for url in ("http://127.0.0.1:17150", "http://localhost:7991/", "http://[::1]:8080", "ws://127.0.0.1:17150/api/live?token=t"):
            self.assertEqual(local_only(url), url.rstrip("/"))
        self.assertEqual(local_hostport("127.0.0.1:17150"), ("127.0.0.1", 17150))

    def test_staging_production_and_other_hosts_are_refused(self):
        for url in (STAGING, "http://op-server-staging-staging.up.railway.app", "https://openpasture.dev", "https://api.openpasture.dev",
                    "http://192.168.1.20:7878", "http://127.0.0.1.nip.io:17150", "https://127.0.0.1:17150", "127.0.0.1:17150", ""):
            with self.assertRaises(NotLocal, msg=url):
                local_only(url)
        with self.assertRaises(NotLocal):
            local_hostport("op-server-staging-staging.up.railway.app:443")

    def test_the_soak_api_refuses_staging_before_any_request(self):
        sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "soak"))
        import farm

        with self.assertRaises(NotLocal):
            farm.Api(STAGING, "token")

    def test_the_soak_driver_exits_before_connecting(self):
        here = os.path.dirname(os.path.abspath(__file__))
        r = subprocess.run([sys.executable, os.path.join(here, "soak", "driver.py"), "--api", STAGING, "--dir", here, "--server-pid", "1"],
                           capture_output=True, text=True, cwd=os.path.join(here, "soak"), timeout=30)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("NotLocal", r.stderr)


if __name__ == "__main__":
    unittest.main()
