import sqlite3


def test_schema_executes_without_error():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)
    conn.commit()

    tables = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
    ).fetchall()
    names = [r[0] for r in tables]
    assert "users" in names
    assert "posts" in names
    assert "comments" in names
    assert "picture" in names
    assert "video" in names
    assert "monitored_users" in names
    assert "sync_history" in names
    assert "comments_sync_progress" in names
    conn.close()


def test_schema_posts_has_expected_columns():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)

    cols = conn.execute("PRAGMA table_info(posts)").fetchall()
    col_names = {r[1] for r in cols}
    for expected in ("id", "uid", "text", "created_at", "retweeted_id",
                     "pic_ids", "pic_infos", "region_name", "source"):
        assert expected in col_names, f"缺少列: {expected}"
    conn.close()


def test_schema_users_has_expected_columns():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)

    cols = conn.execute("PRAGMA table_info(users)").fetchall()
    col_names = {r[1] for r in cols}
    for expected in ("id", "screen_name", "avatar_hd", "domain", "following", "follow_me"):
        assert expected in col_names, f"缺少列: {expected}"
    conn.close()


def test_schema_monitored_users_columns():
    from weiback.models import SCHEMA_SQL
    conn = sqlite3.connect(":memory:")
    conn.executescript(SCHEMA_SQL)

    cols = conn.execute("PRAGMA table_info(monitored_users)").fetchall()
    col_names = {r[1] for r in cols}
    for expected in ("uid", "screen_name", "is_active", "added_at", "last_sync_at"):
        assert expected in col_names
    conn.close()
