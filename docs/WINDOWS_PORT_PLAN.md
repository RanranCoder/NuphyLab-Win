# NuPhyLab Windows 移植方案

状态：M0–M5 与「Windows 增强」批次均已实施（增强批次 QA 见 `WINDOWS_ENHANCEMENTS_QA_20261026.md`；M5 QA 见 `WINDOWS_PACKAGE_QA_20261025.md`，分发指南 `WINDOWS_DISTRIBUTION.md`；真机手测项见 `WINDOWS_SHELL_QA_20261025.md`、`WINDOWS_INPUT_AUDIO_QA_20261025.md`、`WINDOWS_INTEGRATION_QA_20261025.md`） · 日期：2026-10-26 · 基线版本：macOS v1.0.11

## 0. 决策记录

| 决策项 | 结论 |
| --- | --- |
| native 层语言 | **Rust**（单工作区，产出 `HIDBridge.exe` + `NuPhyLab.exe`） |
| HID 访问 | **hidapi**（`hidapi` crate，MSVC 工具链自动编译 vendored C 库） |
| 桌面壳 | **WebView2**（`wry` + `tao`，Tauri 底层组件，可独立使用） |
| 音频 | **WASAPI**（`wasapi` crate；loopback 采集系统声 + 常规捕获麦克风） |
| 进程模型 | **保持 macOS 版三进程模型**：壳 → Node server → HIDBridge.exe，JSONL 协议不变 |
| v1 裁剪 | ① 按键适配层（macOS `UserKeyMapping` 无 Windows 等价物）② Codex 快捷键下发按 Windows 键位重写，放 M4 |
| 兼容原则 | 所有 server/ui 平台改动用 `process.platform === "win32"` 门控，macOS 行为零变更，便于跟随上游 |

## 1. 目标与范围

将 NuPhyLab（Air75 V3 灯光工作室）完整移植到 Windows 10 21H2+ / Windows 11，x64 优先（ARM64 作为可选目标）。功能对齐 macOS 版除明确裁剪项以外的全部能力：逐键调色、动画预设、音频律动（麦克风 + 系统声音）、四款游戏、触发键、Codex/Claude Code 集成、备份恢复。

**不做的事**：不刷固件；不做 OS 级键位重映射（Windows 版用户直接使用键盘 Win 模式，键盘侧 Mac/Win 灯光档位由 A0 handle 机制天然支持，恢复日志已按 profile 0/1 分档）；不做 macOS 签名链的移植。

## 2. 总体架构

```
NuPhyLab.exe (Rust 壳, 对应 native/AppHost.swift)
 ├─ tao 窗口 + wry WebView2，加载 http://127.0.0.1:<port>
 ├─ 托盘图标 (tray-icon)：打开 / 暂停-继续 / 停止并恢复 / 退出
 ├─ 全局热键 (RegisterHotKey) + 低级键盘钩子 (WH_KEYBOARD_LL)
 ├─ SendInput 快捷键注入
 ├─ WASAPI loopback / 麦克风捕获 + FFT → audioPacket
 ├─ Job Object 级联管理 Node 子进程
 │    spawn runtime/node.exe server/main.mjs
 │    env: AIR_STUDIO_HID / AIR_STUDIO_UI / AIR_STUDIO_DATA / AIR_STUDIO_PREFERRED_PORT
 │    stdout 读 {"event":"ready","url":...}
 ▼
Node server（复用，仅平台门控改动）
 │    JSONL over stdio
 ▼
HIDBridge.exe (Rust, 对应 native/HIDBridge.swift)
 ├─ hidapi：枚举/打开/64 字节 output report/输入 report 读回
 ├─ S4 协议引擎：A1/A0/D1/D2/D5/D6/D8/B1/B2/B3 + XOR 会话密钥
 └─ 备份/恢复日志（复用 macOS 版全部语义，含 1.0.23.6 量化反演）
 ▼
Air75 V3 (VID 0x19f5, PID 0x1028)
```

接口合同全部沿用现成文档：JSONL 协议见 `native/PROTOCOL.md`，HTTP API 见 `docs/API.md`。`native/` 目录保留不动，作为移植对照参考。

## 3. Rust 技术选型

| 职责 | crate | 说明 |
| --- | --- | --- |
| HID | `hidapi` | 枚举含 `usage_page`/`usage` 字段，可按 usage page 1 / usage 0 过滤 |
| JSONL | `serde` + `serde_json` | 协议结构体定义对齐 PROTOCOL.md |
| 异步/线程 | `std` + `crossbeam-channel` | HIDBridge 保持与 macOS 版相同的简单队列模型（挂起上限 8） |
| WebView2 | `wry` | IPC 用 `with_ipc_handler`（对应 `codexKeys`/`audioControl` 消息桥），导航白名单用 `with_navigation_handler`，JS 注入用 `evaluate_script` |
| 窗口/事件循环 | `tao` | 窗口尺寸约束（最小 720×520）、WM_POWERBROADCAST 睡眠通知 |
| 托盘 | `tray-icon` | 对应 NSStatusItem |
| 全局热键 | `windows` crate 直接调 `RegisterHotKey` | 简单槽位走热键；两相（按住）与保留键走 `SetWindowsHookExW(WH_KEYBOARD_LL)`，专用线程 + 消息泵 |
| 按键注入 | `windows` crate `SendInput` | 对应 CGEventPost；postToPid 无等价物，改为前台窗口投递 |
| 系统空闲 | `GetLastInputInfo` | 对应 `systemIdle` op |
| 单实例 | 命名 Mutex `Local\Air75StudioHIDBridge` | 对应 flock |
| 音频 | `wasapi` | loopback：`AUDCLNT_STREAMFLAGS_LOOPBACK` 绑定默认渲染端点；麦克风：默认输入端点 |
| FFT | `rustfft` | 2048 点 Hann 窗，输出 RMS + 8 频段（40/80/160/320/640/1280/2560/5120/12000 Hz），≥45ms 节流，对齐 `AudioCapture.swift:7-37` 的 DSP 参数 |
| 日志 | `tracing` + `tracing-subscriber` | stderr 诊断，对齐现有 worker.log 链路 |

## 4. 代码组织

```
native-win/
  Cargo.toml            # [workspace]
  hidbridge/            # → bin/HIDBridge.exe
    src/
      s4/               # S4 协议引擎（帧编解码/XOR 密钥/opcode 路由）——逐函数对照 HIDBridge.swift:321-530
      journal/          # 备份恢复日志（对照 :532-612, 994-1034）
      device.rs         # hidapi 设备管理（枚举/打开/拔出检测）
      ops.rs            # JSONL op 实现（对照 :1578-1669）
      selftest.rs       # --self-test（对照 :1671-2222，不开设备）
  shell/                # → NuPhyLab.exe
    src/
      main.rs           # 进程编排：spawn node.exe、ready 握手、Job Object、优雅退出
      webview.rs        # wry/tao 窗口 + IPC 桥 + 导航白名单 + 对话框
      tray.rs           # 托盘菜单
      hotkey.rs         # RegisterHotKey + WH_KEYBOARD_LL（两相/触发键/保留键）
      inject.rs         # SendInput 快捷键
      audio/            # WASAPI 采集 + DSP
```

npm 脚本新增（不破坏 macOS 脚本）：

```json
"build:native:win": "cd native-win && node ../scripts/cargo-win.mjs build --release && cd .. && node scripts/copy-native-win.mjs",
"selftest:native:win": "cd native-win && node ../scripts/cargo-win.mjs run --release -p hidbridge -- --self-test",
"package:win": "npm run build:icon:win && npm run build:native:win && npm run build:ui && node scripts/package-win.mjs"
```

`scripts/cargo-win.mjs` 把一份可用的 mingw-w64 binutils（dlltool+as）前置到 PATH——rustup windows-gnu 自带的 dlltool 无法派生汇编器，raw-dylib 链接（windows-sys 等）会失败；GitHub Actions runner 自带，本地探测 msys2/mingw-w64 常见安装位（可用 `MINGW_BINS` 追加）。

设备身份：macOS 版用 `locationId` 作重连匹配，Windows 版在 journal 内部改用 hidapi 设备接口路径（同一端口稳定），该字段不出 native 层，不影响 server 协议。

## 5. 里程碑

### M0 — 骨架验证（零 Rust，1-2 天）
- 修 `tests/helpers/fake-service.mjs` 的 shebang spawn 问题：让 `server/hardware.mjs:42` 支持显式 `node <script>` 启动形式（fake helper 产出 `.cmd` 垫片或 hardware 接受命令数组）。此项同时是 Windows CI 的前置。
- Windows 上 `node server/main.mjs` + `AIR_STUDIO_HID` 指向 fake，浏览器跑通 UI 全流程。
- 建 GitHub Actions `windows-latest`：Node 22 + Rust 工具链，`npm test` 全绿（`claude-hooks.test.mjs` 的 `/bin/sh` 用例按平台分支处理）。
- **验收**：测试套件在 Windows CI 全绿；UI 在浏览器可用。

### M1 — HIDBridge.exe MVP（核心风险释放点）
- 移植 S4 协议引擎 + 日志系统 + `--self-test`；实现 connect/status/arm/paint/readColors/side/restore/disconnect/quit。
- `--backup` 参数、profile 0/1 分档日志、`O_EXCL` 原子创建（`CREATE_NEW` + `FlushFileBuffers`）语义对齐。
- **真机验证清单（按序）**：
  1. **report ID 前缀**：Windows `hid_write` 需在 64 字节帧前补 `0x00`（实际 65 字节），`hid_read` 首字节为 report ID——第一优先验证；
  2. 枚举过滤：VID/PID + 65/65 报告长度（真机实测 Windows HidP 对 vendor 接口报告 usage page 0/usage 1，与 macOS IOKit 的 primary usage 1/0 不同，不能照搬）；
  3. 45ms D2 读回屏障时序在 Windows USB 栈上的复现性；
  4. 1.0.23.6 量化反演 + RGB readback 全量校验（`scripts/qa-connected.mjs`、`qa-presets-connected.mjs`、`qa-whole-keyboard.mjs` 直接复用）；
  5. 强杀进程 → 下次连接恢复（Job/断电路径）。
- **验收**：`--self-test` 通过；QA 脚本对真机全绿；拔线/强杀恢复路径符合 PROTOCOL.md「Restoration and lifecycle」章节。

### M2 — WebView2 壳（已实施，QA 见 `WINDOWS_SHELL_QA_20261025.md`）
- 窗口（尺寸约束/记忆）、托盘、spawn node.exe + ready 握手、stderr 旋转日志（1MB）、Job Object、优雅退出（HTTP command 或关 stdin 触发 server 的恢复链，45 秒超时弹窗）、`runtime.json` 握手。
- WebView2 细节：`with_ipc_handler` 实现 `codexKeys`/`audioControl` 双向桥；`with_navigation_handler` 白名单（仅 127.0.0.1 与 blob:，外链走系统浏览器，drive.nuphy.io 保持强制外部浏览器）；`AreDefaultScriptDialogsEnabled` 处理 alert/confirm；退出前 `evaluate_script` 注入 `__AIR_EXITING__` + `nuphylab:shutdown` 事件。
- 实施记录：壳位于 `native-win/shell`（wry 0.47 + tao 0.31 + tray-icon 0.20，windows-gnu）。优雅退出走 stdin EOF（spawn 时设 `AIR_STUDIO_STDIN_EXIT=1`），Job Object 兜底强杀；`codexKeys` 桥 Rust 侧已实现但 JS shim 推迟到 M3（提前暴露会使 UI 关闭页内键盘回退而全局钩子未就绪）；GNU 工具链下 WebView2Loader.dll 动态链接，已 vendor（`native-win/shell/dll/`）随 exe 分发；构建需 mingw-w64 binutils 前置 PATH（rustup 自带 dlltool 无法派生汇编器），由 `scripts/cargo-win.mjs` 包装处理；弹窗用 MessageBoxW 两步式替代 macOS 自定义按钮告警；窗口关闭=隐藏到托盘。开发/测试变量：`AIR_STUDIO_SHELL_ROOT`（资源根覆盖）、`AIR_STUDIO_DATA`（数据目录覆盖）、`AIR_STUDIO_SHELL_QA`（自动探测+自动退出）、`AIR_STUDIO_DEVTOOLS`（F12）。
- **验收**：安装后双击可用；退出时灯效恢复可观测（对照 `docs/QUIT_QA.md`）——前者已达成（根目录布局冒烟），后者列入真机联调。

### M3 — 音频 + 全局按键（已实施，QA 见 `WINDOWS_INPUT_AUDIO_QA_20261025.md`）
- WASAPI loopback（系统声，无 OS 版本门槛）+ WASAPI 麦克风（含 `ms-settings:privacy-microphone` 深链）；`audioInventory`/`audioPacket` 报文与节流对齐；DSP 频段对齐（用 macOS 版录制的灯效表现做 A/B）。
- `WH_KEYBOARD_LL` 实现 `triggerMonitor`（单裸键、listenOnly 语义 = 永远 `CallNextHookEx`）与 `keyMonitor`（Ctrl+Alt+1…6/N）；**键码映射层**：协议线上继续用 macOS keyCode（避免改动 `shared/trigger-keys.json` 与 server），HIDBridge 内部维护 mac↔VK 映射表。
- 实施记录：新增 `native-win/keymap`（Carbon↔VK 全量正向表 + 无歧义反向表，两侧共用）。监听与注入在 **HIDBridge**（`hidbridge/src/hooks.rs`，与 macOS 的 KeyHooks/TriggerHooks 同位，server 协议零改动；§2 架构图把 SendInput 画在壳层，实际以 JSONL 合同为准）——LL 钩子各占独立线程+消息泵，`LLKHF_INJECTED` 过滤自身注入，Windows 无 autorepeat 标志以 down 集合去重；`shortcut` op 用 SendInput（globalHold/globalTap 和弦含 alreadyHeld 释放，120s 兜底释放，fn 修饰键诚实拒绝）。壳层（`shell/src/hotkey.rs`）单 LL 钩子承担 Codex 12 键两相/按住、F1–F12 录制捕获、编辑/聚焦门控、`codexKeyStatus` 上报（registered=全部绑定、failed 恒空）与 500ms 轮询节流；`codexKeys` shim 已暴露。音频（`shell/src/audio.rs`，wasapi crate + rustfft）：loopback=默认渲染端点、麦克风=默认/指定捕获端点，float32@混音格式，2048 点 Hann + 8 频段 + 45ms 节流 + RMS（单测含 440Hz 落带校验），包合并分发（2s 超时）+ 4s 清单节流 + 1s 静默保活 + 会话代际看门狗；poll 缺答不当作禁用（BUG_AUDIT P2-2）。
- **验收**：音频律动 10 种效果表现一致；触发键延迟与 macOS 版相当——两项均需真机，列入联调清单。

### M4 — Codex / Claude Code 集成（已实施，QA 见 `WINDOWS_INTEGRATION_QA_20261025.md`）
- Codex CLI 发现换 Windows 安装位置；`execFile("/usr/bin/sqlite3")` → `node:sqlite`（只读）；lsof 写者检测降级返回 null（server 已容忍）；`app-server --stdio` 用量查询保留（注意 `.cmd` 需要 `shell:true`）。
- SHORTCUTS 表按 Windows Codex 实际键位重写（Ctrl 系 + VK 码）；两相听写手势用 LL 钩子 + `GetAsyncKeyState` 重建。
- Claude hook：`.cmd`/直调 node 启动器、`HOOK_MARKER` 与安装命令成套替换、node 候选列表换 Windows 路径、桌面会话目录 `%APPDATA%\Claude`。
- **验收**：hook 安装/卸载幂等；会话灯效与任务跳转在真机可用。
- 实施记录：SHORTCUTS 表保持 macOS 词汇为源，Windows 派生（Command→Ctrl、Option→Alt，显示串 `Ctrl+Alt+1` 加号连接）；线协议 modifiers 仍用 macOS 词汇（`option`），Rust 侧 `parse_flags` 两边都收，避免动 shared 与 keymap。`cmdorctrl`/`commandorcontrol` 解析按平台落 Ctrl/⌘。Codex CLI 候选：`%LOCALAPPDATA%\Programs\Codex\codex.exe`、ChatGPT resources、`.codex\bin`、winget Links、npm `.cmd` 垫片（`.cmd` spawn 加 `shell:true`，stop 时 taskkill /t /f 防孤儿）。`readLiveWriters` win32 返回 null（事件侧活跃度照常）。Claude 动作：`claude://` 深链走 `cmd /d /s /c start`；focusApp 直调 `%LOCALAPPDATA%\AnthropicClaude\claude.exe`（Electron 单实例，launch 后不等待）；revealProject 用 explorer.exe（先 access 校验，退出码忽略）；剪贴板用 `clip.exe`；copyResume 命令串 `cd /d "<path>" &&`。Claude hook：Windows 直调 node + worker `.mjs`（HOOK_MARKER 平台化，安装时钉死 process.execPath，漂移由 needsRepair 兜底），去 chmod；node 候选补 `%APPDATA%\npm\node.exe`。桌面会话目录 `%APPDATA%\Claude`（`defaultClaudeSupportDir`）。`key-adaptation` v1 在 win32 整层禁用（`syncKeyAdaptation` 门控 + codexAdapt 不再写 codexKeyAdaptation=true，CodexSetup 对话框隐藏改键区）。`claude-sessions` 进程存活维持 M2 时代的 signal-0 探测（返回 null 起始时间，pid 复用比对自动跳过），未引入 PowerShell CIM。UI：设备页权限卡换 Win 模式提示（win32）、设备详情平台标注、「这台 Mac」文案中性化；字体栈补 Segoe UI / Microsoft YaHei；i18n 补 20 条 Windows 文案 en 词条。
- **真机待验**：Codex Windows 版快捷键表（Ctrl 系默认是否与实际版本一致，`%LOCALAPPDATA%\Programs` 下实际安装名）；Claude 桌面版 Windows 安装路径；hook 安装后真实 Claude Code 会话上报。

### M5 — 打包、签名、发布（已实施，QA 见 `WINDOWS_PACKAGE_QA_20261025.md`）
- `fetch-node.mjs` → win-x64 zip（去掉 otool，改 PE 头校验）；`package.mjs` → 目录布局（`NuPhyLab.exe` + `runtime/node.exe` + `bin/HIDBridge.exe` + `ui/dist` + `server/`）+ **Inno Setup** 安装包（同步出便携 zip）。
- 图标 `.ico`（`png2icons`，弃用 icon.swift）；**Authenticode 代码签名**（HID + 全局钩子类程序无签名极易被 SmartScreen/杀软误报，此为分发硬门槛）。
- 新增 `docs/WINDOWS_DISTRIBUTION.md` 与 Windows 版 QA 记录文档（对齐 `docs/RELEASE_QA_*.md` 体例）。
- **验收**：干净虚拟机（Win10 21H2 / Win11）安装 → 全功能 → 卸载，无残留进程。
- 实施记录：`fetch-node.mjs` 平台矩阵化（`runtimeArchive` 纯函数），win 解压用 `System32\tar.exe`（bsdtar 支持 zip）回退 Expand-Archive，`verifyRuntime` win 分支为 PE 头校验（MZ/PE 签名 + 0x8664 机器类型），darwin 的 otool 逻辑原样保留。`build-icon:win` 用 png2icons（根 devDependency，纯 JS）出 `assets/AppIcon.ico`；shell 增 `build.rs` + winresource 把图标与 VERSIONINFO（package.json 版本四段式打包）嵌进 exe，windres 由 cargo-win.mjs 的 mingw binutils 提供，图标缺失时 warning 降级。`signing-win.mjs`：signtool 发现（PATH → Windows Kits 按版本逐段降序）、证书选择（PFX env > AIR_STUDIO_SIGN_IDENTITY > 存储中唯一 Code Signing 证书；PowerShell 只读枚举）、签名单 NuPhyLab.exe + HIDBridge.exe（node.exe/WebView2Loader.dll 保留原签名）、`AIR_STUDIO_ALLOW_UNSIGNED=1` 显式跳过（signing.json adhoc 如实标记，禁止对外分发）。`package-win.mjs`：布局组装 + LAYOUT_SPEC 完整性校验（空文件/空目录/缺失分列）+ 打包内 node 启动验证 + signing.json + Compress-Archive 便携 zip；ISCC 检测（AIR_STUDIO_ISCC > Program Files (x86)/(x64)）未装则优雅跳过 setup.exe。`installer-win.iss`：per-user 免 UAC、稳定 AppId、装/卸前 taskkill（不碰通用 node.exe）。main.rs webview 失败分支加 WebView2 Evergreen 缺失引导（ask_yes_no + 官方下载页）。
- **真机待验**：真实证书签名全链路与 SmartScreen 行为；ISCC 出包与干净虚拟机安装/卸载/升级；WebView2 缺失引导（Win10 无运行时虚拟机）。

### M6 — Windows 增强（已实施，QA 见 `WINDOWS_ENHANCEMENTS_QA_20261026.md`）

在移植完成之后新增的四项能力。前三项补齐桌面应用基本盘，第四项是 Windows 独有的产品差异化；均不改变 macOS 代码路径（平台门控或纯新增）。

| 能力 | 实现要点 | 关键文件 |
| --- | --- | --- |
| 登录自启 | `HKCU\…\CurrentVersion\Run` 的 `NuPhyLab` 值（带引号绝对路径），托盘勾选项；启动时自愈旧安装路径；删除不存在的值是幂等成功 | `shell/src/autostart.rs`、`traymenu.rs` |
| 睡眠/唤醒 | 独立线程 + message-only 窗口收 `WM_POWERBROADCAST`：`PBT_APMSUSPEND` 停音频与屏幕采集，恢复由 0.5 s 轮询重启（对齐 macOS willSleep） | `shell/src/power.rs`、`main.rs` |
| 系统主题 | 设置项 `appearance`（浅色/深色/跟随系统，**默认浅色**，侧栏下拉切换）；壳只发布系统值（`AppsUseLightTheme` + DWM `ColorizationColor`，4 s 轮询）到 `data-os-theme-system`，UI 用 `effectiveTheme` 决定最终主题；CSS 21 处字面白底改为变量以支持暗色 | `shared/appearance.mjs`、`shell/src/theme.rs`、`ui/src/App.tsx`、`styles.css`/`studio-design.css` |
| 屏幕氛围光 | DXGI Desktop Duplication → staging → 6×16 区域平均（与键盘几何一致）→ `screenPacket` 复用音频会话生命周期；三种效果（映射/整体/边缘）+ 增益与平滑 | `shell/src/capture.rs`、`shared/screen-ambient.mjs`、`server/audio.mjs`、`ui/src/AudioPanel.tsx` |

- 屏幕取色只输出区域平均值，像素不出 native 层、不落盘、不上传；UI 文案与页脚双重说明。
- 实测抓出的两处 silent-failure：`CopyResource` 格式不匹配（HDR/10-bit 桌面）导致全黑 → 改 `IDXGIOutput5::DuplicateOutput1` 显式请求 BGRA8；竖屏 surface 尺寸与桌面 rect 不符 → staging 取 `ModeDesc` 尺寸。
- 诊断入口：`AIR_STUDIO_SCREEN_PROBE=<秒>`（区域网格写入 `screen-probe.txt`）、`AIR_STUDIO_AUTOSTART_QA=1`（注册表往返自检并恢复原状态）、QA 探针新增 `osTheme`/`accent`/`darkPanel` 字段。
- **真机待验**：屏幕源实际点灯、睡眠/唤醒日志与采集恢复、深浅色实时切换、与录屏软件共存时的错误提示（详见 QA 文档联调清单）。

## 6. 逐文件改造清单（server / ui / scripts / tests / hooks）
### server/（全部 `process.platform === "win32"` 门控）

| 文件:行 | 现状 | Windows 处理 |
| --- | --- | --- |
| `server/main.mjs:35-37` | 数据目录 `~/Library/Application Support/Air Studio` | `%APPDATA%\Air Studio` |
| `server/main.mjs:46` | 默认 `bin/HIDBridge` | `bin/HIDBridge.exe` |
| `server/main.mjs:55, 824` | macOS 权限/监听提示文案 | 平台分支文案 |
| `server/main.mjs:1330-1331` | 仅 SIGTERM/SIGINT | 补 `SIGBREAK` + stdin EOF 关闭（已实现：Windows 下监听 `SIGBREAK`；宿主设 `AIR_STUDIO_STDIN_EXIT=1` 显式启用 stdin EOF 优雅退出，关管道即触发恢复链） |
| `server/audio.mjs:15` | “系统声音需要 macOS 14.2+” | 平台分支（Windows 无版本门槛） |
| `server/claude-actions.mjs:14, 107-117` | `/Applications/Claude.app` | `%LOCALAPPDATA%\AnthropicClaude\claude.exe` |
| `server/claude-actions.mjs:129-156` | `open`/`pbcopy` | `start`/`explorer.exe`/`clip`；`copyResume` 命令串改 `cd /d "<path>" && claude --resume <id>` |
| `server/claude-actions.mjs:144` | ⚠️ `startsWith("/")` 判绝对路径 | 接受盘符路径（不改则 Windows 静默失效） |
| `server/claude-events.mjs:37` | ⚠️ cwd 必须 `/` 开头 | 接受盘符路径（同上） |
| `server/claude-hooks.mjs:22-24, 77-81` | POSIX hook 命令 + chmod + node 候选 | Windows hook 命令、去 chmod、node 候选换 `%APPDATA%\npm\node.exe` 等 |
| `server/claude-sessions.mjs:19-38, 88` | `/bin/ps` + `~/Library/.../Claude` | PowerShell `Get-CimInstance Win32_Process`；`%APPDATA%\Claude` |
| `server/codex.mjs:384-390, 545` | macOS CLI 候选 + `/usr/bin/sqlite3` | Windows 候选列表（`.exe`/`.cmd`）；`node:sqlite` |
| `server/codex-writers.mjs:15, 31` | `lsof` + `ps` | 返回 null（已容忍）/ PowerShell CIM |
| `server/codex-shortcuts.mjs:7-25` | `parseMacShortcut` Mac 虚拟键码 | M4 重写为 Windows 键位 |
| `server/key-adaptation.mjs` | macOS 键位映射层 | v1 整层禁用（平台门控 + UI 隐藏入口） |
| `server/hardware.mjs:42, 251-254` | spawn fake/shebang；SIGTERM 兜底 | 支持 `node <script>` 启动形式；Windows 下兜底语义变为 TerminateProcess，恢复可靠性由 stdin EOF 路径保证 |

### ui/ 与 shared/

| 位置 | 处理 |
| --- | --- |
| `ui/src/Device.tsx:152-168` | Mac 权限卡整块平台分支（Windows 换成 Win 模式提示 + 竞争软件提示） |
| `ui/src/Device.tsx:174` | 平台显示加 `win32` 分支 |
| `ui/src/App.tsx:153`、`Device.tsx:181`、`CodexSetup.tsx:56` | “这台 Mac”类文案平台分支 |
| `ui/src/styles.css:3, 1363` | 字体栈补 `"Segoe UI", "Microsoft YaHei"` |
| `shared/i18n.mjs` | 约 15 条 macOS 文案加平台分支（:67, 96, 145, 181, 201, 203, 341, 350, 402, 732, 744, 1074-1077） |
| `shared/layout.mjs:76-78` | 保留（⌘/Opt 是物理 Mac 键帽事实标注） |

### scripts/ tests/ hooks/

| 位置 | 处理 |
| --- | --- |
| `scripts/fetch-node.mjs` | win-x64 矩阵 + PE 校验 |
| `scripts/package.mjs` / 新 `package-win.mjs` | Inno Setup 布局 |
| `scripts/build-icon.mjs` + `icon.swift` | 新 `build-icon:win` → `.ico`（png2icons） |
| `scripts/signing.mjs` | 新 `signing-win.mjs` → signtool；`signing.json` 机制保留 |
| `tests/helpers/fake-service.mjs:126` | shebang spawn 修复（M0） |
| `tests/claude-hooks.test.mjs:26` | `/bin/sh` 用例平台分支 |
| `tests/packaging-runtime.test.mjs:28` | `vendor/node-arm64` 断言按平台矩阵 |
| `tests/native-hotkeys.test.mjs` / `tests/audio-native.test.mjs` | darwin 跳过守卫已存在；为 Rust 侧新增对等测试（见 §7） |
| `hooks/nuphylab-claude-hook.sh` | 新增 `.cmd`/直调 node 变体，与 `claude-hooks.mjs` 安装逻辑成套 |

## 7. Windows 特有风险与对策

| 风险 | 对策 |
| --- | --- |
| report ID 前缀差异（65 字节写） | M1 第一验证项；hidapi 层封装 `write_frame/read_frame` 统一处理 |
| 无 POSIX 信号，SIGTERM 兜底退化为强杀 | 恢复链主路径改走 stdin EOF + `quit` op；磁盘日志兜底强杀（现有设计天然覆盖，PROTOCOL.md:111 语义不变）；Job Object 保证不残留孤儿进程 |
| 设备拔出检测 | hidapi 读写失败 + 周期性 status 探测（Windows 无 IOKit removal callback 等价物，hid_hotplug 不支持 Windows） |
| 蓝牙枚举 | hidapi 枚举 + 路径含 `#Bluetooth#` 判传输层 + product 正则（对齐 `HIDBridge.swift:17-24`），只枚举不开设备，语义不变 |
| 杀软/SmartScreen 误报 | Authenticode 签名；发布前在 Defender 实机验证 |
| WebView2 运行时依赖 | Win11 内置；Win10 用 Evergreen loader + 缺失检测引导下载 |
| 读回时序漂移 | 45ms 屏障与重试参数做成常量，真机标定后固化（对齐 PROTOCOL.md「verification」章节的 fail-closed 原则） |
| 1.0.23.6 量化反演依赖固件行为 | 移植时逐字节对照 Swift 实现，M1 用同一把已验证固件的键盘做 RGB readback 回归 |

## 8. 测试策略

- **Rust 单元/自测**：S4 帧编解码、XOR 密钥推导、路由校验、日志迁移移植为 `cargo test`（对照 Swift `--self-test` 用例集，HIDBridge.swift:1671-2222）。
- **JSONL 合同测试**：新增 Node 测试驱动 HIDBridge.exe（fake 设备不可用时跳过真机段），对齐 PROTOCOL.md 每个 op。
- **现有套件**：M0 后全部在 Windows CI 跑通（fake-service 注入机制不变，这正是保持三进程模型的红利）。
- **真机 QA**：复用 `scripts/qa-*.mjs` 四个脚本 + `native/qa-hardware.py` 思路（Windows 版可用 Python 或改 Node），结果落 `docs/` QA 文档。
- **音频 A/B**：macOS 版录制灯效表现作基准，Windows 版对齐频段能量分布。

## 9. 待定项

- 安装包最终选型（Inno Setup 为默认建议，若需进 Microsoft Store 则改 MSIX，两者可并存）。
- Windows ARM64 是否进第一版（Node 与 Rust 均有 aarch64 目标，成本主要在真机验证）。
- Codex Windows 版快捷键表需以实际安装版本为准（M4 时核对）。
