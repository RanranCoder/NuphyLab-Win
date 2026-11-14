# Windows 壳(M2)QA 记录 · 2026-10-25

环境:Windows 11(10.0.26300)· rustc 1.99.1 stable-x86_64-pc-windows-gnu · node.exe 26.12(PATH)· WebView2 运行时为系统内置。开发机同时有一个 14:40 起的活跃 start.cmd 会话(持有键盘设备与 `original-lighting.json` 备份日志),所有隔离测试均通过 `AIR_STUDIO_DATA` 指向临时目录,未触碰该会话与其数据。

## 构建链

- `scripts/cargo-win.mjs` 解决 GNU 工具链的 raw-dylib 前置条件:rustup 自带的 self-contained `dlltool` 无法派生汇编器,必须把一份完整 mingw-w64 binutils(msys2 `mingw64/bin`)置于 PATH 首位;GitHub Actions 的 windows runner 自带,本地由包装脚本探测。已纳入 `npm run build:native:win` / `selftest:native:win`。
- wry 0.47 + tao 0.31 + tray-icon 0.20 在 windows-gnu 编译链接通过;GNU 下 WebView2Loader 为动态链接(MSVC 静态),DLL 已 vendor 至 `native-win/shell/dll/`,由 copy-native-win.mjs 随 `NuPhyLab.exe` 摆放到仓库根。
- `bin/HIDBridge.exe` 被活跃会话锁定时拷贝脚本跳过并警告(不硬失败);hidbridge `--self-test` 5321 项通过,工作区未改动其构建配置。

## 自动冒烟(AIR_STUDIO_SHELL_QA=1,3 秒桥探测 + 8 秒自动走托盘退出链)

对 debug 与 release(根目录布局,不设 `AIR_STUDIO_SHELL_ROOT`,验证 exe 旁资源解析)各一轮,结果一致:

- ready 握手 → WebView2 加载,页面标题 NuPhyLab;
- 桥注入探针:`ipc: "object"`(wry 原生)、`audioBridge: true`、`codexKeysExposed: false`(见"已知偏差"第 1 条)、`exiting: false`;
- 退出链:stdin EOF → server 恢复链 → `Worker exited: status=0` → 进程 ExitCode=0,无弹窗;Job Object 下无孤儿 node/HIDBridge 进程;
- `Logs/worker.log` 依次记录 Worker started / QA 探针 / Worker exited,旋转与 64KiB 截断逻辑随代码移植(大数据量轮转未触发)。

## 备份保留路径(真实数据目录)

数据目录存在未恢复的 `original-lighting.json`(14:40 会话产物)时退出:bridge 以 code=2 结束(备份保留语义,`hidbridge/src/main.rs:115`)→ server `hardware.close()` 记 `键盘控制进程退出异常(code=2)` 并以 status=1 退出 → 壳弹「未能确认键盘恢复」警告(含日志路径与"打开日志所在文件夹"后续询问)。与 macOS fail-visible 语义一致(同一 server 代码路径),非壳缺陷。

## 已知偏差与真机手测清单

1. `codexKeys` 桥的 Rust 侧(消息解析、config 校验、状态存储)已实现,但 JS shim 暂不暴露该 handler:UI 一旦探测到它就关闭页内键盘回退,而全局键捕获(WH_KEYBOARD_LL)属 M3,提前暴露会使 Codex 快捷键全部失效。M3 落地钩子后翻转 `webview.rs` 中一处常量即接入。
2. 退出警告弹窗用 MessageBoxW 两步式(主告警 + "打开日志文件夹"是/否追问),Win32 标准消息框无法自定义按钮文案;45 秒"仍在恢复"弹窗走独立线程,不阻塞 WorkerExited 事件。
3. 窗口关闭按钮 = 隐藏到托盘(对齐 macOS `applicationShouldTerminateAfterLastWindowClosed=false`);退出仅从托盘菜单/托盘左键打开窗口。
4. 下载落 `Downloads/NuPhyLab-<unix>-<净化名>`;失败下载清理残留文件。未实机触发,真机手测项。
5. 窗口状态记忆(`window-state.json`)仅在移动/缩放后节流落盘,带虚拟屏幕越界丢弃;headless 冒烟未产生移动事件,真机手测项。
6. 外链白名单(仅 127.0.0.1 与 blob:)、drive.nuphy.io 的 Chrome→Edge→默认浏览器优先、`ms-settings:privacy-microphone` 深链:代码就绪,真机手测项。
7. 托盘「暂停 / 继续」「停止并恢复」命令与语言切换(/api/state 0.5s 轮询 → 重建托盘菜单)为代码路径,真机手测项。
8. MSYS bash 的 `timeout` 不等待 GUI 子系统进程,验证退出码需 PowerShell `Start-Process -Wait`(工具链事实,非产品问题)。

## 结论

M2 的进程编排、ready 握手、IPC 桥、导航白名单、优雅退出链在 Windows 上按预期工作,验收项"双击可用"达成(根目录布局);"退出时灯效恢复可观测"需真机(对照 `docs/QUIT_QA.md`),列入 M3 真机联调清单一并执行。下一步 M3:WASAPI 音频 + WH_KEYBOARD_LL 全局键 + codexKeys shim 接入。
