"""uuid7 生成器测试。"""

import re
import unittest

from weiback_collector.uuid7 import uuid7

UUID_V7_RE = re.compile(
    r"^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
)


class Uuid7Test(unittest.TestCase):
    def test_format_matches_schema_pattern(self):
        for _ in range(200):
            self.assertRegex(uuid7(), UUID_V7_RE)

    def test_all_generated_are_unique(self):
        seen = {uuid7() for _ in range(1000)}
        self.assertEqual(len(seen), 1000)

    def test_version_and_variant_bits(self):
        for _ in range(50):
            value = uuid7()
            version = int(value[14], 16)
            variant = int(value[19], 16)
            self.assertEqual(version, 7)
            self.assertGreaterEqual(variant, 8)
            self.assertLessEqual(variant, 11)


if __name__ == "__main__":
    unittest.main()
