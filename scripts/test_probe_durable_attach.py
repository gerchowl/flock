"""Failure ceilings for the slow-sync probe's producer handshake."""

import time
import unittest

from scripts.probe_durable_attach import wait_for


class Producer:
    def __init__(self, status=None):
        self.status = status

    def poll(self):
        return self.status


class WaitTests(unittest.TestCase):
    def test_live_silent_producer_fails(self):
        started = time.monotonic()
        with self.assertRaisesRegex(TimeoutError, "silent producer"):
            wait_for(lambda: False, Producer(), "sync", timeout=0.05)
        self.assertLess(time.monotonic() - started, 2)

    def test_dead_producer_fails_even_if_marker_exists(self):
        with self.assertRaisesRegex(RuntimeError, "server exited"):
            wait_for(lambda: True, Producer(1), "sync")

    def test_real_condition_completes(self):
        wait_for(lambda: True, Producer(), "sync", timeout=0.05)
