"""Small independent checks for the resource campaign's checksum/interruption oracle."""

from __future__ import annotations

import concurrent.futures
import hashlib
import tempfile
import unittest
from pathlib import Path

from large_campaign import MIB, SegmentReader, generate_payload


class CampaignChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "payload"

    def test_dense_checksum_oracle_matches_all_written_bytes(self):
        expected = generate_payload(self.path, 3 * MIB + 7)
        data = self.path.read_bytes()
        self.assertEqual(len(data), 3 * MIB + 7)
        self.assertEqual(hashlib.sha256(data).hexdigest(), expected)
        self.assertNotEqual(data[:MIB], data[MIB : 2 * MIB])
        self.assertNotEqual(data[MIB : 2 * MIB], data[2 * MIB : 3 * MIB])

    def test_seekable_segment_cannot_leak_adjacent_part_bytes(self):
        generate_payload(self.path, 3 * MIB)
        expected = self.path.read_bytes()[MIB : 2 * MIB]
        with SegmentReader(self.path, MIB, MIB) as reader:
            self.assertEqual(reader.read(), expected)
            self.assertEqual(reader.read(1), b"")
            reader.seek(-5, 2)
            self.assertEqual(reader.read(20), expected[-5:])
            with self.assertRaises(ValueError):
                reader.seek(MIB + 1)

    def test_signed_stream_rewind_then_real_prefix_pause_preserves_bytes(self):
        generate_payload(self.path, 3 * MIB)
        expected = self.path.read_bytes()[MIB : 3 * MIB]
        with SegmentReader(self.path, MIB, 2 * MIB) as reader:
            # SDK signing is allowed to consume the complete segment before arm.
            self.assertEqual(
                hashlib.sha256(reader.read()).digest(),
                hashlib.sha256(expected).digest(),
            )
            reader.arm()
            prefix = reader.read(MIB)
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                future = executor.submit(reader.read, MIB)
                self.assertTrue(reader.paused.wait(2))
                self.assertFalse(future.done())
                self.assertEqual(reader.delivered, MIB)
                reader.release.set()
                suffix = future.result(timeout=2)
            self.assertEqual(prefix + suffix, expected)
            self.assertEqual(reader.read(1), b"")


if __name__ == "__main__":
    unittest.main()
