# Windows 发布与安装完整性

本项目的 Windows 发布门禁由 `scripts/verify-release.ps1` 执行。它不会修改
`target/release/bundle` 中的产物；哈希清单、报告、MSI 管理安装提取和 NSIS 临时
安装均写入指定的工作目录。删除该工作目录即可清理验证状态。

## 发布前提

- 三处版本必须相同：工作区 `Cargo.toml`、`tauri-app/src-tauri/tauri.conf.json`、
  `tauri-app/package.json`。
- 发布身份固定为 `WeiBack Next` / `com.weiback.next` / `weiback-next.exe`，并使用
  `weiback-next` 数据和配置命名空间。不得回退为旧版 `com.weiback.app`。
- Windows 安装器固定为当前用户安装；同一标识的后续版本应由安装器升级，旧版与
  Next 因 identifier 不同必须并存。
- Sidecar 必须由 `sidecar/weiback-collector.spec` 打成 onefile EXE，并以
  `binaries/weiback-collector-x86_64-pc-windows-msvc.exe` 交给 Tauri `externalBin`。

## 构建

在 Windows x64 环境中执行：

```powershell
Set-Location sidecar
python -m pip install pyinstaller
pyinstaller --noconfirm weiback-collector.spec
Copy-Item dist/weiback-collector.exe ..\tauri-app\src-tauri\binaries\weiback-collector-x86_64-pc-windows-msvc.exe -Force

Set-Location ..\tauri-app
yarn install --immutable
yarn tauri build
```

预期产物在 `tauri-app/src-tauri/target/release/`：主 EXE、带 target triple 的
Sidecar，以及 `bundle/` 下的一个 `.msi` 与一个 NSIS `-setup.exe`。

## CI 测试门禁

`release.yml` 的 Windows tag 发布和 `release-integrity.yml` 的手动验证均在生成
Sidecar、Tauri 安装器、签名或上传前依次执行以下门禁：

```powershell
$env:CARGO_BUILD_JOBS = '1'
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

Set-Location sidecar
python -m pip install --upgrade pip
python -m pip install -e .
$env:PYTHONPATH = (Get-Location).Path
python -m unittest discover -s tests -t . -v

Set-Location ..\tauri-app
yarn build
yarn lint
```

CI 固定使用 Python 3.12。先安装 Sidecar 项目及其依赖并运行 unittest，再安装
PyInstaller 打包，避免打包工具安装掩盖项目依赖或测试失败。门禁顺序的静态回归检查：

```powershell
.\scripts\test-release-workflow-gates.ps1
```

## 候选与正式门禁

候选模式允许未签名产物，只用于构建链路和安装完整性候选验证；输出会记录每个文件
的签名状态，`Unsigned` 不是正式发布的放行条件。

```powershell
.\scripts\verify-release-candidate.ps1
```

该快捷入口在尚未构建产物时只检查版本、身份和 UpgradeCode，并输出
`configuration-only` 报告；产物存在时会验证完整候选。CI 不使用这个宽松入口，
构建后缺少任何一类产物即失败。

正式发布必须在签名完成后运行，并要求主 EXE、Sidecar、MSI、NSIS 的
`Get-AuthenticodeSignature` 状态均为 `Valid`，且签名证书匹配
`scripts/release-identity.json` 的 `approvedSigners` 策略：

```powershell
.\scripts\verify-release.ps1 -Mode release
```

验证器会检查：版本与永久身份、四类产物、SHA-256 清单、Sidecar `hello` 后的
`ready/capabilities` 以及 `shutdown` 退出、MSI 管理安装提取与 NSIS 临时安装中的
同哈希 Sidecar。失败即阻塞发布。报告默认写入 `%TEMP%\weiback-release-verify-*`；
CI 使用 runner 临时目录并上传 JSON 报告。

## 签名

在构建主程序、Sidecar 和安装器后，为四个最终文件分别执行组织批准的
Authenticode 签名流程，并在签名后运行正式门禁。签名证书、私钥、时间戳凭据不得
写入仓库、日志或 GitHub Actions 输出。

Tauri 可通过 `bundle.windows.signCommand` 在构建时调用组织签名工具；当前配置不
内置任何证书或签名命令。`approvedSigners` 必须至少提供一个精确证书指纹或主题正则。
受控 CI 可以用 `WEIBACK_APPROVED_SIGNER_THUMBPRINTS` 临时覆盖指纹列表，但基础策略
仍不可为空。无有效且获批准的签名时只能运行 `candidate`，不得上传为正式 Release。
tag 工作流在同一个 Windows job 中先构建、签名并完成严格门禁，只有成功后才调用
上传步骤。`tauri-action` 在该 job 只构建、不接收 release 参数；严格门禁成功后由
`gh release upload` 上传刚验证过的 Windows 资产。未接入签名环境的 tag 会故意失败，
不能绕过正式门禁。

## 升级、并存与卸载验证

1. 在隔离 Windows 用户或 VM 中安装当前正式版，再安装同 identifier 的更高版本。
   确认它走升级路径，`com.weiback.next` 与 `weiback-next.exe` 不变。
2. 同时保留旧版 `com.weiback.app`。确认两套快捷方式可启动，且 `%APPDATA%`/数据
   根下的 `weiback-next` 不与旧 `weiback` 命名空间重合。
3. 卸载 Next 后确认旧版安装和旧数据仍在；卸载旧版后确认 Next 安装和
   `weiback-next` 配置、session、数据库、媒体、日志仍在。
4. 自动门禁的 NSIS 安装仅使用工作目录 `/D=<temp>`，随后运行其中的 `uninstall.exe /S`；
   MSI 仅进行 `/a` 管理安装提取。因此脚本不写入用户安装目录、配置目录或数据目录。

`scripts/test-windows-install-lifecycle.ps1` 是 P4 生命周期验证。它隔离
`APPDATA` 和 `LOCALAPPDATA` 到 `WorkDir`，以 NSIS 静默安装 Next，校验主 EXE 与
Sidecar 共存，写入 Next 数据哨兵；提供旧版安装器时还会安装 Legacy、写入旧数据哨兵、
校验并存、卸载并重装 Next 后两套数据和旧版安装仍完整，最后无论成功或失败都会尝试
卸载两者。全部安装、卸载和可选 `-Smoke` 启动都有超时、终止和等待保障。

```powershell
.\scripts\test-windows-install-lifecycle.ps1 `
  -NextInstaller .\tauri-app\src-tauri\target\release\bundle\nsis\weiback-next-setup.exe `
  -LegacyInstaller C:\artifacts\weiback-legacy-setup.exe `
  -Smoke
```

未传 `-LegacyInstaller` 时脚本会明确输出 `SKIP`，仅完成 Next 安装验证，绝不将并存
检查报告为通过。tag 发布工作流要求仓库变量 `WEIBACK_LEGACY_INSTALLER_URL` 指向可下载的
旧版 NSIS 资产；变量缺失、下载失败或生命周期检查失败均会阻止正式 Release 上传。
脚本自身的解析与守卫检查可用 `powershell -File
.\scripts\test-test-windows-install-lifecycle.ps1` 执行。

MSI UpgradeCode 的静态基线可用 Tauri CLI 核对：

```powershell
Set-Location tauri-app
yarn tauri inspect wix-upgrade-code
```

当前基线为 `13a0729c-a277-55ef-828c-495a6d30aa6d`。记录输出到发布审批单；任何
identifier 或 productName 的变动必须先更新 `scripts/release-identity.json`、ADR-005
和升级验证证据，不能在发布当日临时改变。

## 备份、恢复与回滚

- 安装器升级与卸载不等同于数据备份。发布前从应用内导出数据库和媒体备份，并单独
  保存到不在应用数据目录的位置。
- 恢复时先退出应用，再使用应用支持的导入/恢复流程；不要把旧版目录直接覆盖到
  `weiback-next`，旧数据仅通过一次性快照导入契约迁移。
- 回滚到上一个 Next 版本前先备份当前 `weiback-next` 数据。仅安装先前安装包不会
  自动回滚数据库 schema 或任务状态；若当前版本已迁移数据，应从兼容备份恢复。
- 回滚不应安装旧版 `com.weiback.app` 来替代 Next。两者是永久独立产品身份。
