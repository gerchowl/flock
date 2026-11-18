"""Failure ceilings for the slow-sync probe's producer handshake."""

import os
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest

from scripts.probe_durable_attach import attach, painted_workspace, read_line_until, wait_for


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


class ResponseDeadlineTests(unittest.TestCase):
    def test_trickled_bytes_cannot_extend_whole_response_deadline(self):
        reader, writer = socket.socketpair()
        stop = threading.Event()
        arrived = threading.Event()
        errors = []

        def trickle():
            try:
                writer.settimeout(0.2)
                trickle_deadline = time.monotonic() + 0.65
                while not stop.is_set() and time.monotonic() < trickle_deadline:
                    writer.sendall(b"x")
                    arrived.set()
                    if stop.wait(0.01):
                        break
                # Stay alive but silent, with a ceiling even for a broken reader.
                stop.wait(2)
            except OSError as error:
                errors.append(error)

        producer = threading.Thread(target=trickle)
        producer.start()
        try:
            self.assertTrue(arrived.wait(1), "trickle producer stayed silent")
            started = time.monotonic()
            with self.assertRaisesRegex(TimeoutError, "whole-response deadline"):
                read_line_until(reader, started + 0.1)
            self.assertLess(time.monotonic() - started, 0.5)
            self.assertTrue(producer.is_alive(), "deadline must beat a live producer")
            self.assertFalse(errors)
        finally:
            stop.set()
            producer.join(timeout=1)
            reader.close()
            writer.close()
        self.assertFalse(producer.is_alive())

    def test_complete_response_is_read(self):
        reader, writer = socket.socketpair()
        try:
            writer.sendall(b'{"result":true}\n')
            self.assertEqual(read_line_until(reader, time.monotonic() + 1), b'{"result":true}')
        finally:
            reader.close()
            writer.close()

    def test_unterminated_response_fails_on_eof(self):
        reader, writer = socket.socketpair()
        try:
            writer.sendall(b"partial")
            writer.shutdown(socket.SHUT_WR)
            with self.assertRaises(EOFError):
                read_line_until(reader, time.monotonic() + 1)
        finally:
            reader.close()
            writer.close()

    def test_expired_deadline_refuses_already_readable_bytes(self):
        reader, writer = socket.socketpair()
        try:
            writer.sendall(b"complete\n")
            with self.assertRaises(TimeoutError):
                read_line_until(reader, time.monotonic() - 1)
        finally:
            reader.close()
            writer.close()

    def test_byte_limit_is_bounded(self):
        reader, writer = socket.socketpair()
        try:
            writer.sendall(b"over limit")
            with self.assertRaises(ValueError):
                read_line_until(reader, time.monotonic() + 1, limit=4)
        finally:
            reader.close()
            writer.close()


class FirstFrameTests(unittest.TestCase):
    PAINT = {"target": "flock::attach", "side": "client", "stage": "first_paint"}

    def test_terminal_setup_is_not_a_frame(self):
        self.assertFalse(painted_workspace(b"\x1b[?1049h", []))

    def test_window_title_is_not_visible_workspace_content(self):
        self.assertFalse(painted_workspace(b"\x1b]2;sync-probe\x07", [self.PAINT]))
        self.assertFalse(painted_workspace(b"\x1b]2;sync-probe\x1b\\", [self.PAINT]))

    def test_frame_must_be_flushed_and_contain_workspace(self):
        self.assertFalse(painted_workspace(b"sync-probe", []))
        self.assertFalse(painted_workspace(b"", [self.PAINT]))
        self.assertTrue(painted_workspace(b"\x1b[3;1Hsync-\x1b[32mprobe", [self.PAINT]))

    def test_live_peer_with_only_terminal_setup_fails_at_ceiling(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "silent-client"
            binary.write_text("#!/usr/bin/env python3\nimport os\nos.write(1, b'\\x1b[?1049h')\nos.read(0, 1)\n")
            binary.chmod(0o700)
            env = dict(os.environ, XDG_CONFIG_HOME=directory)
            started = time.monotonic()
            result = attach(binary, env, timeout=0.5)
            self.assertFalse(result["attached"])
            self.assertIsNotNone(result["welcome_seconds"])
            self.assertIsNone(result["first_frame_seconds"])
            self.assertIsNone(result["exit"], "producer must remain alive until probe cleanup")
            self.assertLess(time.monotonic() - started, 2)
