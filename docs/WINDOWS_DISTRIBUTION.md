# NuPhyLab Windows 分发指南

适用于 Windows 10 21H2+ / Windows 11，x64。构建机需要：Node ≥ 22（PATH）、Rust windows-gnu 工具链 + 一份 mingw-w64 binutils（`scripts/cargo-win.mjs` 自动探测，可用 `MINGW_BINS` 追加）。

## 构建与打包

```cmd
npm run setup            && rem 安装 ui 依赖（首次）
npm run package:win
```

`package:win` 依次执行：图标（`build:icon:win`，png2icons 由 logo 生成 `assets/AppIcon.ico`，同时经 `winresource` 嵌入 NuPhyLab.exe）→ Rust 原生（`build:native:win`，产出 `bin/HIDBridge.exe`、`NuPhyLab.exe`、`WebView2Loader.dll`）→ UI（`build:ui`）→ 打包（`scripts/package-win.mjs`）。

打包前需要官方 Node 运行时（执行一次 `npm run fetch-node`，下载 win-x64 zip 并做 SHASUMS256 + PE 头校验，落 `vendor/node-x64/node.exe`；`AIR_STUDIO_NODE` 可指定其他文件）。

产物在 `release/win/`：

| 产物 | 说明 |
| --- | --- |
| `NuPhyLab-win-x64/` | 目录布局（便携版本体）：`NuPhyLab.exe` + `WebView2Loader.dll` + `runtime/node.exe` + `bin/HIDBridge.exe` + `server/ shared/ hooks/ ui/dist/` + `signing.json` + README/LICENSE |
| `NuPhyLab-win-x64-v<版本>.zip` | 便携 zip（Compress-Archive） |
| `NuPhyLab-win-x64-v<版本>-setup.exe` | Inno Setup 安装包（仅当安装了 Inno Setup 6；用 `AIR_STUDIO_ISCC` 指定 ISCC.exe 路径） |

包内 `signing.json` 记录版本、平台、签名身份与打包时间；server 把它透出为 `/api/state` 的 build 信息（与 macOS 同一机制）。

## 代码签名（分发硬门槛）

HID 访问 + 全局键盘钩子类程序未签名极易被 SmartScreen/杀软误报，对外分发必须 Authenticode 签名。`scripts/signing-win.mjs`：

| 方式 | 设置 |
| --- | --- |
| PFX 文件 | `AIR_STUDIO_SIGN_PFX=<路径>` + `AIR_STUDIO_SIGN_PFX_PASSWORD=<密码>` |
| 指纹/主题（用户或本机证书存储） | `AIR_STUDIO_SIGN_IDENTITY=<SHA-1 指纹或主题>` |
| 存储中恰好一张代码签名证书 | 自动选用（PowerShell 枚举 `Cert:\CurrentUser\My` + `Cert:\LocalMachine\My`，仅取有私钥 + Code Signing EKU 的证书） |
| signtool 位置 | 自动：PATH → Windows Kits `bin\<版本>\x64`；`AIR_STUDIO_SIGNTOOL` 可显式指定 |
| 时间戳 | 默认 `http://timestamp.digicert.com`（/tr + /td SHA256）；`AIR_STUDIO_SIGN_TIMESTAMP=0` 关闭（离线构建） |
| 跳过签名 | `AIR_STUDIO_ALLOW_UNSIGNED=1`——**仅供本机开发验证，产物不得对外分发**，signing.json 的 `adhoc` 会如实标记 |

签名单（signtool sign /fd SHA256 + 时间戳，随后 verify /pa /all）：`NuPhyLab.exe`、`bin/HIDBridge.exe`。`runtime/node.exe` 保持 Node 官方构建原样；`WebView2Loader.dll` 保持 Microsoft 原签名，均不重签。

证书要求：OV/IV 以上代码签名证书即可（EV 证书可立即累积 SmartScreen 信誉，OV 需要下载量积累）；证书必须包含 Code Signing EKU 与私钥。

## 安装包行为（installer-win.iss）

- per-user 安装（`{localappdata}\Programs\NuPhyLab`），无 UAC；HID 与钩子均不需要管理员。
- 安装与卸载前 taskkill `NuPhyLab.exe` / `HIDBridge.exe`（壳的 Job Object 会级联收掉 node 子进程；绝不杀通用 node.exe——用户可能跑着自己的 Node 进程）。
- 可选桌面图标；默认开始菜单项；安装完成可勾选直接启动。
- 升级覆盖安装靠稳定 AppId（GUID）识别，与版本号、证书无关。

## WebView2 运行时

Win11 内置；Win10 需 Evergreen 运行时。缺失时壳在 WebView 创建失败处弹窗引导打开官方下载页（<https://developer.microsoft.com/microsoft-edge/webview2/>），安装后重新运行即可。分发时无需捆绑 bootstrapper；如需离线安装包可自行附带 `MicrosoftEdgeWebview2Setup.exe`。

## 发布前检查单

1. 干净虚拟机（Win10 21H2 / Win11 各一）：安装 → 全功能（连接、灯效、音频、触发键、游戏、退出恢复）→ 卸载，确认无残留进程与残留文件（`%LOCALAPPDATA%\Programs\NuPhyLab`、开始菜单、桌面）。
2. 已签名产物在另一台未安装证书的机器上：SmartScreen 提示行为记录在案（OV 证书初期会提示「更多信息 → 仍要运行」）。
3. 便携 zip 解压到含空格/中文路径可运行。
4. 升级安装：旧版本运行中直接装新版，确认 taskkill 后覆盖成功、数据目录（`%APPDATA%\Air Studio`）保留。
5. 病毒总（ VirusTotal / Defender 实机）误报扫描记录。

## 与 macOS 打包的差异

| 项 | macOS | Windows |
| --- | --- | --- |
| 布局 | .app bundle（Contents/Resources） | exe 旁平铺目录 |
| 签名 | codesign（ entitlements + Developer ID/ad-hoc） | signtool Authenticode（无 entitlements 概念） |
| 权限重置 | ad-hoc 重签导致 TCC 授权失效 | 无 TCC；签名只影响 SmartScreen 信誉 |
| 安装包 | 手动拖拽（.app） | Inno Setup（可选）+ 便携 zip |
| docs/ 目录 | 打进包内 | 不打入（内部 QA 记录不随包分发） |
