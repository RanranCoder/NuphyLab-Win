# Windows 集成(M4)QA 记录 · 2026-10-25

环境与 M2/M3 记录相同(Windows 11 · node 26.12 · 隔离数据目录)。本机未安装 Codex 桌面版与 Claude 桌面版(已探测:`where codex` 无结果、无 `%LOCALAPPDATA%\AnthropicClaude`、无 npm 全局 codex/claude),自动化覆盖离线分支、协议合同与安装器幂等;依赖真实安装的项列入联调清单。

## Codex 侧

- SHORTCUTS 表:macOS 词汇为源,Windows 派生 Command→Ctrl、Option→Alt;显示串 `Ctrl+Alt+1`(加号连接,无 ⌘⌥ 符号);20 个动作全量解析通过,线 modifiers 保持 macOS 词汇(`option`),与 HIDBridge `parse_flags`(command|cmd、control|ctrl、option|alt、shift、fn)逐一对上。
- `cmdorctrl`/`commandorcontrol` 按平台解析:win32 → `control`,darwin → `command`(对齐 Codex 自己写入 keybindings.json 的平台化习惯)。
- CLI 发现:显式路径优先;否则 `%LOCALAPPDATA%\Programs\Codex\codex.exe` → ChatGPT resources → `.codex\bin` → winget Links → npm `codex.cmd`。`.cmd`/`.bat` 后缀时 spawn 加 `shell:true`,stop 走 `taskkill /pid <pid> /t /f`(树杀,防止 cmd 包装留下 node 孤儿);其余路径仍 `SIGTERM`。
- `node:sqlite` 只读分支已在 M0–M3 期间落地(win32 `sqlBuiltin`,跳过 `/usr/bin/sqlite3`),本批补齐配套;`readLiveWriters` win32 直接返回 null(无 lsof),事件侧活跃判定不受影响,pollTasks 对 null writerState 已容忍。
- 额度链路(spawn app-server → initialize → account/rateLimits/read)无法在本机验证,等待真实 Codex 安装。

## Claude Code 侧

- hook 安装器:Windows 直调 node + worker `.mjs`(`HOOK_MARKER` 平台化为 `.mjs`;命令串 `"node" "...nuphylab-claude-hook.mjs" "...spool"`,路径不能含双引号故普通双引号安全);安装时钉死 `process.execPath`,安装位置漂移由 status() 的 needsRepair 语义兜底(与 macOS 的 stale command 同一路径);不写执行位(POSIX chmod 跳过);node 候选补 `%APPDATA%\npm\node.exe`。
- 行为套件(安装幂等、备份、他人 hooks 保留、并发不串行、payload 容错、stdin 不关闭 5s 内退出、disableAllHooks 上报)现已在 Windows 上真实运行:重写 `tests/claude-hooks.test.mjs`,行为用例直调 node worker,仅 shell 引号探针保留 POSIX skip。
- 动作执行:`claude://code/continue?session=local_*` 深链走 `cmd /d /s /c start "" "<url>"`(session id 先过 `^local_[A-Za-z0-9-]{1,64}$`,URL 惰性);focusApp 直调 `%LOCALAPPDATA%\AnthropicClaude\claude.exe`(Electron 单实例语义,不等待退出);revealProject 先 `access` 校验再 explorer.exe(退出码非零仍视为成功);剪贴板 `clip.exe`;copyResume 串 `cd /d "<path>" && claude --resume <id>`。
- 计划中标注的两处 ⚠️(`claude-actions.mjs:144`、`claude-events.mjs:37` 的 `startsWith("/")`)核实为已用 `path.isAbsolute`,盘符路径天然通过,无需修改。
- 会话注册表:`readProcessStarts` 维持 signal-0 探测(存活→起始时间 null,pid 复用比对自动跳过),未引入 PowerShell CIM;`defaultClaudeSupportDir` → `%APPDATA%\Claude`。
- 服务级深链测试(fake runner 记录载体调用)按平台断言:win32 记录 cmd.exe + `start ""`,darwin 记录 `/usr/bin/open --`。

## 键位适配层(v1 裁剪)与服务文案

- `syncKeyAdaptation` win32 门控 `enabled:supported&&…`;`codexAdapt` 不再把 `codexKeyAdaptation` 写成 true;UI CodexSetup 在 win32 隐藏改键行/键帽回退选择,按钮改「连接与检查」。Rust 桩此前已拒绝 `adaptKeys`/`readKeymap`(fail-closed),两侧一致。
- 文案平台分支:快捷键失败提示(Windows 无辅助功能权限概念)、触发键降级警告(Windows 拒绝 LL 钩子时);i18n 补 20 条 en 词条,并把「在这台 Mac 上播放的声音」「保存在这台 Mac」两条 en 值中性化为 computer(macOS 中文键名不变,零行为变更)。
- 设备页 win32 权限卡:键盘访问恒允许(HIDBridge permission_snapshot 恒 true)+ Win 档拨杆提示 + 竞争软件排查帮助;设备详情平台标注 `Windows x64`。

## 测试结果

- 全套 `npm test`:330 项,319 过,0 失败,11 平台跳过(darwin 专属:8 个服务级键位适配/蓝牙适配流转用例加 `platform!=='darwin'` 守卫;`native-hotkeys`/`audio-native` 原有守卫;shell 引号探针)。
- 新增:`tests/claude-actions.test.mjs` 4 项(候选发现、copyResume 平台串与 clip/pbcopy 载体、深链/focusApp/explorer 载体、focusApp 无安装诚实报错);`codex-shortcuts` 平台默认表全量解析 + CmdOrCtrl 平台落点 1 项;`claude-hooks` 平台命令形态 1 项。
- UI `tsc -b && vite build` 通过(1623 模块,无类型错误)。

## 已知限制与真机联调清单(M4 验收项)

1. Codex Windows 版实际安装位置与默认快捷键表未验证(候选列表按标准安装布局书写);`Ctrl+Alt+1..6/N`、两相听写、`codex://threads/` 跳转需真实 Codex。
2. Claude 桌面版 Windows 路径(`%LOCALAPPDATA%\AnthropicClaude\claude.exe`)与 `claude://` 协议注册行为未验证;hook 安装后的真实会话上报(含 `%APPDATA%\Claude` 桌面 id 映射)未验证。
3. `keybindings.json` 自定义键里 Windows 版可能出现的 `CmdOrCtrl`/`Super` 写法:前者已按平台解析,后者(Windows 键)解析为 `command` 且 HIDBridge 会照发——真实 Codex 是否用 Win 键做全局加速器待核,若出现应视为需要用户改绑的冲突项。
4. 联调时连同 M3 清单第 3、4 项(Ctrl+Option+1..6/N 槽位、shortcut 注入真实 Codex)一并执行。
