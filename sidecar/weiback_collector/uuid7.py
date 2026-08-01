"""UUID v7 生成器。

Python stdlib 不提供 UUID v7，这里按 RFC 9562 布局实现：

- 48 位 Unix 时间戳（毫秒）
- 4 位版本号（7）
- 12 位 rand_a（同毫秒内单调递增，保证生成顺序稳定）
- 2 位 variant（10）
- 62 位 rand_b

返回标准字符串形式，与协议 schema 的 `uuid_v7` pattern 一致。
"""

from __future__ import annotations

import os
import threading
import time
import uuid

_lock = threading.Lock()
_last_ms: int = -1
_last_rand_a: int = 0


def uuid7() -> str:
    """返回一个新的 UUID v7 字符串。线程安全，同毫秒内单调递增。"""
    global _last_ms, _last_rand_a
    now_ms = int(time.time() * 1000)
    rand_a = int.from_bytes(os.urandom(2), "big") & 0x0FFF
    rand_b = int.from_bytes(os.urandom(8), "big") & 0x3FFFFFFFFFFFFFFF

    with _lock:
        if now_ms == _last_ms:
            _last_rand_a = (_last_rand_a + 1) & 0x0FFF
            rand_a = _last_rand_a
        else:
            _last_ms = now_ms
            _last_rand_a = rand_a

    raw = bytearray(16)
    raw[0:6] = now_ms.to_bytes(6, "big")
    raw[6] = 0x70 | (rand_a >> 8)  # version = 7
    raw[7] = rand_a & 0xFF
    raw[8] = 0x80 | ((rand_b >> 56) & 0x3F)  # variant = 10
    raw[9:16] = (rand_b & 0x00FFFFFFFFFFFFFF).to_bytes(7, "big")
    return str(uuid.UUID(bytes=bytes(raw)))
