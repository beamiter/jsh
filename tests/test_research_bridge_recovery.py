"""Fault injection around stop/resume durability and stale proposal epochs."""
from pathlib import Path
from unittest.mock import patch

from test_research_bridge import BridgeFixture


class ResumeDurabilityTests(BridgeFixture):
    def test_state_write_failure_does_not_clear_stop(self):
        self.baseline()
        self.bridge.snapshot()
        self.store.stop()
        before = self.store.load()
        with patch.object(self.bridge.store, "save", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.bridge.resume()
        self.assertTrue(self.store.stopped())
        self.assertEqual(self.store.load(), before)

    def test_stop_unlink_failure_leaves_new_epoch_and_closed_campaign(self):
        self.baseline()
        self.bridge.snapshot()
        self.store.stop()
        before = self.store.load()["revision"]
        original = Path.unlink
        def fail_stop(path, *args, **kwargs):
            if path.name == "STOP":
                raise OSError("simulated unlink failure")
            return original(path, *args, **kwargs)
        with patch.object(Path, "unlink", fail_stop):
            with self.assertRaises(OSError):
                self.bridge.resume()
        self.assertTrue(self.store.stopped())
        self.assertGreater(self.store.load()["revision"], before)
        self.bridge.resume()
        self.assertFalse(self.store.stopped())
