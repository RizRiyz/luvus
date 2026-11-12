"""Deterministic startup readiness tests; no real processes or sleeps."""
import os
import pathlib
import stat
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

import live_conformance as live


class SocketReadinessTests(unittest.TestCase):
    def info(self, mode=0o600, kind=stat.S_IFSOCK, uid=None):
        return SimpleNamespace(st_mode=kind | mode,
                               st_uid=os.geteuid() if uid is None else uid)

    def wait(self, observations, *, deadline=0.06, process_status=None):
        clock = [0.0]
        endpoint = Mock(spec=pathlib.Path)
        endpoint.lstat.side_effect = observations
        process = Mock()
        process.poll.return_value = process_status
        sleeps = []

        def advance(duration):
            self.assertGreater(duration, 0)
            self.assertLessEqual(clock[0] + duration, deadline)
            sleeps.append(duration)
            clock[0] += duration

        with patch.object(live.time, "monotonic", side_effect=lambda: clock[0]), \
             patch.object(live.time, "sleep", side_effect=advance):
            live.wait_owner_socket(endpoint, process, deadline)
        return endpoint, sleeps

    def test_bind_visible_before_chmod_does_not_count_as_ready(self):
        endpoint, sleeps = self.wait([FileNotFoundError(), self.info(0o755),
                                     self.info(0o600)])
        self.assertEqual(endpoint.lstat.call_count, 3)
        self.assertEqual(sleeps, [0.025, 0.025])

    def test_ready_endpoint_does_not_sleep(self):
        _, sleeps = self.wait([self.info()])
        self.assertEqual(sleeps, [])

    def test_wrong_owner_and_type_fail_immediately(self):
        for info in [self.info(uid=os.geteuid() + 1),
                     self.info(kind=stat.S_IFLNK),
                     self.info(kind=stat.S_IFREG)]:
            with self.subTest(info=info):
                endpoint = Mock(spec=pathlib.Path)
                endpoint.lstat.return_value = info
                process = Mock()
                process.poll.return_value = None
                with patch.object(live.time, "monotonic", return_value=0), \
                     patch.object(live.time, "sleep") as sleep:
                    with self.assertRaisesRegex(RuntimeError, "wrong owner|not a Unix socket"):
                        live.wait_owner_socket(endpoint, process, 10)
                    sleep.assert_not_called()

    def test_permanent_bad_modes_are_bounded_by_original_deadline(self):
        for mode in [0o755, 0o660, 0o400, 0o1600]:
            with self.subTest(mode=mode):
                with self.assertRaisesRegex(RuntimeError, "startup deadline"):
                    self.wait([self.info(mode)] * 3)

    def test_absent_endpoint_times_out_without_expanding_deadline(self):
        with self.assertRaisesRegex(RuntimeError, "startup deadline"):
            self.wait([FileNotFoundError()] * 3)

    def test_expired_deadline_or_exited_process_never_accepts_socket(self):
        with self.assertRaisesRegex(RuntimeError, "startup deadline"):
            self.wait([self.info()], deadline=0)
        with self.assertRaisesRegex(RuntimeError, "server exited"):
            self.wait([self.info()], process_status=1)

    def test_unexpected_filesystem_error_is_not_retried(self):
        with self.assertRaises(PermissionError):
            self.wait([PermissionError("denied")])


if __name__ == "__main__":
    unittest.main()
