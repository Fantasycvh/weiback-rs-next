import sqlite3
import tempfile
from pathlib import Path
from typing import Iterator

import pytest


@pytest.fixture
def db_path() -> Iterator[str]:
    with tempfile.NamedTemporaryFile(suffix=".db", delete=False) as f:
        path = f.name
    yield path
    try:
        Path(path).unlink(missing_ok=True)
    except PermissionError:
        pass


@pytest.fixture
def conn(db_path: str) -> Iterator[sqlite3.Connection]:
    from weiback.writer import connect
    c = connect(db_path)
    yield c
    c.close()
