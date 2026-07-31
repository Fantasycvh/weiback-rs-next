import json
import logging
import os
import sys
from dataclasses import dataclass, asdict, field
from pathlib import Path

logger = logging.getLogger(__name__)


@dataclass
class Config:
    db_path: str = ""
    port: int = 8080
    interval_minutes: int = 30
    max_pages: int = 0
    page_delay: float = 3.0
    with_comments: bool = False
    download_dir: str = ""
    version: int = 1


def get_default_config_path() -> Path:
    home = Path.home()
    if sys.platform == "win32":
        return home / "AppData" / "Roaming" / "weiback" / "config.json"
    if sys.platform == "darwin":
        return home / "Library" / "Application Support" / "weiback" / "config.json"
    return home / ".config" / "weiback" / "config.json"


def load_config(config_path: str | None = None) -> Config:
    path = Path(config_path) if config_path else get_default_config_path()
    cfg = Config()
    if path.exists():
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            for k, v in data.items():
                if hasattr(cfg, k):
                    setattr(cfg, k, v)
            logger.info("已加载配置: %s", path)
        except Exception as e:
            logger.warning("配置加载失败，使用默认值: %s", e)
    else:
        _init_default_db_path(cfg)
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(asdict(cfg), ensure_ascii=False, indent=2), encoding="utf-8")
            logger.info("已创建默认配置: %s", path)
        except Exception as e:
            logger.warning("配置写入失败: %s", e)
    return cfg


def save_config(cfg: Config, config_path: str | None = None) -> None:
    path = Path(config_path) if config_path else get_default_config_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(asdict(cfg), ensure_ascii=False, indent=2), encoding="utf-8")
        logger.info("配置已保存: %s", path)
    except Exception as e:
        logger.warning("配置保存失败: %s", e)


def _init_default_db_path(cfg: Config):
    if not cfg.db_path:
        home = Path.home()
        if sys.platform == "win32":
            cfg.db_path = str(home / "AppData" / "Roaming" / "weiback" / "weiback.db")
        elif sys.platform == "darwin":
            cfg.db_path = str(home / "Library" / "Application Support" / "weiback" / "weiback.db")
        else:
            cfg.db_path = str(home / ".local" / "share" / "weiback" / "weiback.db")