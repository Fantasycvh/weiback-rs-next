# WeiBack（微博备份工具）

基于 Python 的微博数据备份与监控工具：定时增量同步关注博主的微博内容，完整抓取正文、图片、视频与**全量评论**（含多级回复树），并通过内置 Web GUI 浏览、检索与导出。

## 功能特性

- **定时增量同步**：监控多位博主，按 `last_id` 断点增量抓取，不重复全量翻页
- **手动抓取范围**：可指定用户、内容类型（全部/原创/图片/视频/长文）、页数、是否带评论
- **评论全量抓取**：走 `weibo.com/ajax/statuses/buildComments` 按 `max_id` 游标翻页，非仅最近 20 条；自动递归抓取二级回复（`m.weibo.cn/comments/hotFlowChild`）
- **评论树结构**：评论按 `root_id / parent_id / depth` 建树，Web 端递归展示多级回复
- **媒体管理**：图片/视频/头像统一入库，支持一键后台下载落盘
- **Web GUI**：概览、监控用户、微博列表（搜索/筛选/分页）、帖子详情、手动抓取
- **任务跟踪**：后台任务状态与错误可通过 API 轮询

## 快速开始

### 源码运行

要求 Python 3.11+：

```bash
pip install -r requirements.txt
playwright install chromium      # 首次需下载浏览器（用于获取登录 Cookie）
```

### 命令行

```bash
# 守护模式 + Web GUI + 定时同步
python weibo_monitor.py --db-path ./weiback.db --daemon

# 仅启动 Web GUI（不启动同步）
python weibo_monitor.py --db-path ./weiback.db --serve-only

# 单次同步所有监控用户后退出
python weibo_monitor.py --db-path ./weiback.db --now

# 添加监控用户
python weibo_monitor.py --db-path ./weiback.db --add-user 123456 --user-name "博主昵称"

# 回补已收录帖子的评论（可配合 --limit 控制每批数量）
python weibo_monitor.py --db-path ./weiback.db --backfill --limit 50

# 下载未落盘的媒体文件
python weibo_monitor.py --db-path ./weiback.db --download-images
```

浏览器打开 `http://127.0.0.1:8080`。

### CLI 参数

| 参数 | 说明 |
|---|---|
| `--config` | 配置文件路径（默认自动检测） |
| `--db-path` | SQLite 数据库路径 |
| `--now` | 单次同步后退出 |
| `--daemon` | 守护模式 + Web GUI + 定时同步 |
| `--port` | Web GUI 端口（默认 8080） |
| `--interval` | 同步间隔分钟数（默认 30） |
| `--with-comments` | 同步时一并抓取评论 |
| `--max-pages` | 每用户最大页数（0=不限） |
| `--page-delay` | 翻页间隔秒数（默认 3.0，控制频次防风控） |
| `--backfill` | 回补已有帖子的评论 |
| `--download-images` | 下载未落盘的图片 |
| `--download-dir` | 图片下载目录 |
| `--limit` | 回补评论数量上限 |
| `--serve-only` | 仅启动 Web GUI |
| `--add-user` | 添加监控用户 (UID) |
| `--user-name` | 添加监控用户时的昵称 |

## Web GUI

| 页面 | 功能 |
|---|---|
| 概览 `/` | 同步状态、统计、立即同步 |
| 监控用户 `/users` | 添加/移除监控用户 |
| 微博列表 `/posts` | 关键词、用户、日期、内容状态筛选 + 分页 |
| 帖子详情 `/posts/{id}` | 正文、转发原文、媒体、多级评论树、单帖回补评论 |
| 手动抓取 `/backup` | 按用户/内容类型/页数/评论条数后台抓取 |

主要 API：

| 接口 | 说明 |
|---|---|
| `POST /backup/start` | 启动手动抓取（后台任务） |
| `POST /posts/{id}/fetch-comments` | 单帖回补评论 |
| `POST /comments/{root}/replies` | 抓取某条一级评论的回复 |
| `POST /sync/now` | 立即同步所有监控用户 |
| `POST /api/download/images` | 后台下载未落盘媒体 |
| `GET /api/task/status` | 当前任务与错误状态 |

## 数据与存储

- **数据库**：SQLite（`--db-path` 指定，默认 `~/AppData/Roaming/weiback/weiback.db`）
- **主要表**：`posts`、`comments`（含 `root_id/parent_id/depth`）、`media`、`users`、`monitored_users`、`sync_history`、`comments_sync_progress`、`comment_reply_progress`
- **评论进度游标**：每帖记录 `max_id` 游标与已抓数量，中断后下次自动续抓
- **图片下载目录**：默认数据库同级的 `images/`，Web 端通过 `/images/` 静态映射访问

## 配置

首次运行自动生成配置文件（`~/AppData/Roaming/weiback/config.json`）：

```json
{
  "db_path": "",
  "port": 8080,
  "interval_minutes": 30,
  "max_pages": 0,
  "page_delay": 3.0,
  "with_comments": false,
  "download_dir": ""
}
```

命令行参数优先级高于配置文件。

## 构建与打包

```bash
pip install -r requirements.txt pyinstaller
playwright install chromium
pyinstaller weibo-monitor.spec --clean
```

输出 `dist/weibo-monitor.exe`（约 60MB，含 Web GUI 模板）。详见 [BUILD.md](BUILD.md)。

> 注意：`weibo-monitor.exe` 首次启动需要可访问 Playwright Chromium（`setup_playwright()` 会自动定位或提示安装）。

## 免责声明

- 本项目仅供个人数据备份使用，请遵守微博平台的服务条款与当地法律法规
- 请合理控制抓取频率（默认 `page_delay=3.0`），高频抓取可能触发平台风控
- 发布的构建未做签名，Windows 可能提示未知发布者

## 许可证

个人使用项目，保留所有权利。请勿用于商业用途。
