# 构建指南

## 前置条件

```bash
pip install -r requirements.txt
pip install pyinstaller
playwright install chromium
```

## 打包

```bash
pyinstaller weibo-monitor.spec --clean
```

输出：`dist/weibo-monitor.exe` (~40MB)

## 使用

```bash
# 守护模式 + Web GUI
dist/weibo-monitor --db-path ./weiback.db --daemon

# 仅启动 Web GUI（不启动同步）
dist/weibo-monitor --db-path ./weiback.db --serve-only
```

浏览器打开 `http://127.0.0.1:8080`。

## NSSM 注册为 Windows 服务

```bash
nssm install WeiBack "dist/weibo-monitor.exe" --db-path C:\data\weiback.db --daemon
nssm start WeiBack
```
