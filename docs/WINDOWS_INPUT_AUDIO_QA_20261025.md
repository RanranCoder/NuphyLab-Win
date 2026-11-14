# Windows 输入与音频(M3)QA 记录 · 2026-10-25

环境与 M2 记录相同(Windows 11 · rustc 1.99.1 windows-gnu · 隔离数据目录)。本里程碑无真机键盘参与,自动化覆盖离线分支与协议合同;真机表现项列入联调清单。

## 键码映射(native-win/keymap)

- 正向表覆盖 shared/trigger-keys.json 全部 74 个 Carbon 码 + 功能键/小键盘/修饰键;反向表仅无歧义打字键集(小键盘 Enter 与主 Enter 共享 VK_RETURN、F13+ 与小键盘数字块冲突,故仅正向)。
- 单测 6 项:覆盖完整性、无歧义集往返一致、拒绝歧义反查、F 键名、修饰词汇表(与 server/codex-shortcuts.mjs 的 modifiers 映射对齐)、Codex 槽位码与默认触发键(空格)。

## HIDBridge 输入 op(真实 JSONL 协议,spawn 进程逐条验证)

8/8 通过:keyMonitor 禁用回 `{enabled:false}`;triggerMonitor 空格(49)启用成功;keyCode 10(白名单外)→ `INVALID_TRIGGER_KEY`;触发键带 suppress/modifiers → `INVALID_TRIGGER_MONITOR`;keyMonitor 修饰非 Ctrl+Option → `INVALID_BINDING`;shortcut 目标非 Codex → `INVALID_TARGET`;shortcut phase=up 无按住 → `{released:false}`;quit 干净退出(exit 0,钩子全部卸载)。

- `--self-test` 5353 项通过(新增 32 项:触发键校验/shouldTrigger 谓词/keyMonitor 默认与七上限/修饰解析/shortcut 纯校验分支)。
- 监听器各占独立线程与消息泵;`LLKHF_INJECTED` 过滤自身 SendInput;Windows LL 钩子无 autorepeat 标志,以物理 down 集合去重;triggerMonitor 永不抑制(listenOnly 语义),keyMonitor 仅在绑定带 suppress 时抑制匹配事件;门控翻转时无条件清除 down 标记避免"粘滞"。shortcut 注入:普通/两相/调制弦三路径,alreadyHeld 先释后按(ShortcutChord 对齐),120s 兜底释放;`fn` 修饰键无 Windows 等价物,诚实报 `INVALID_SHORTCUT`(fail-closed);任务跳转 `codex://threads/<id>` 经 ShellExecute,threadId 字符集与 1..160 长度校验对齐。

## 壳层热键与音频

- 冒烟(QA 模式,release):`codexKeysExposed: true`(shim 已接入)、`audioBridge: true`、ready 后 LL 钩子安装、退出链 status=0 / ExitCode=0。
- 音频分析器单测 4 项:静音零能量、440Hz 正弦落 320–640Hz 频段且不泄漏高频、45ms 节流、半缓冲累积。Hann/频段边界/RMS/归一化逐式对齐 AudioFeatureAnalyzer.swift(vDSP),仅窗函数归一化常数可能存在无关紧要的标度差(引擎侧自有归一)。
- WASAPI:loopback = 默认渲染端点 + LOOPBACK 标志(wasapi crate 以 Direction::Render 表达),麦克风 = 默认/指定捕获端点;请求 float32@混音率 + 自动格式转换;轮询读(4ms)——loopback 静默时无回调,1s 保活 listening 状态包;`audioInventory` 4s 节流直发(设备重枚举,不复用陈旧缓存,规避 BUG_AUDIT P3-24 模式);`audioPacket` 合并到最新 + 2s 超时;会话代际计数器兜底(server 消失→poll 退出→停采集);/api/state 缺答不当作禁用(P2-2)。
- server/audio.mjs:15 文案平台分支已加(win32 下该分支实际不可达,防御性)。

## 真机联调清单(M3 验收项)

1. 音频律动 10 种效果与 macOS 录制表现 A/B(需键盘 + 播放音频;loopback 与麦克风各一轮)。
2. 触发键端到端延迟(空格默认键;keyDown→triggerKey 事件→server fireTrigger→灯效),与 macOS 版相当性。
3. Ctrl+Option+1..6/N 全局槽位(需 Codex 集成或观察 server codexKey 事件流);Codex 12 键两相/按住/录制捕获实机验证。
4. shortcut 注入到真实 Codex Windows 版(目标窗口枚举按 `codex.exe` 进程名,版本以 M4 核对为准);`codex://threads/` 跳转。
5. M2 遗留手测项(WINDOWS_SHELL_QA_20261025.md §真机手测清单)一并执行。

## 已知限制

- 麦克风权限:Windows 桌面应用无逐应用查询 API,inventory 恒报 `microphonePermission: true`;若系统隐私开关拒绝采集,初始化失败将按错误码上报(inputUnavailable)。
- 系统待机唤醒(WM_POWERBROADCAST 停采)未接入;会话代际与 poll 重启路径覆盖其后果,真机验证后决定是否补。
- keyMonitor 抑制路径(suppress=true)在真机按住场景下的焦点边界待联调。

## 首轮真机缺陷与修复(2026-10-25 下午)

用户实测系统声音报 `systemCapture`("系统声音暂不可用,请检查录音权限后重试")。根因是 wasapi crate 的 loopback 约定:**渲染设备 + 初始化参数传 `Direction::Capture`** 才会设置 `AUDCLNT_STREAMFLAGS_LOOPBACK`;初版把渲染方向也传给了 `initialize_client`,loopback 标志被静默丢弃,对渲染客户端请求采集服务即 `AUDCLNT_E_WRONG_ENDPOINT_TYPE`。同时该 crate 恒设 `EVENTCALLBACK`,初始化后必须注册事件句柄(即便 loopback 不触发事件、靠轮询读取)。修复:初始化参数恒为 Capture;初始化后 `set_get_eventhandle`;loopback 改用端点混音格式原样初始化(不再带转换标志,混音格式即 float32);所有失败路径补写 `[audio]` 诊断日志(此前失败路径无日志,定位只能靠排除法)。新增 `examples/loopback-probe.rs` 独立探针,实测默认渲染端点(FreeBuds,44100Hz float32)捕获 4.65s 音频、峰值 0.54,数据流验证通过。
