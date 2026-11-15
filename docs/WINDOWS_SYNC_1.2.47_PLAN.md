# Windows 同步 Mac 端 v1.2.47 移植计划

> 基线：Windows 移植自 macOS v1.0.11（当前 Win 版本 1.0.12）。
> 目标：将 Mac 端 [bohu8264/NuPhyLab-next](https://github.com/bohu8264/NuPhyLab-next) v1.2.47 的全部功能同步到 Windows 版（用户已确认全量同步 + 完整实现 Win 更新器）。
> Mac 参考仓库本地克隆：`d:\MyApp\_mac_ref`（main = ce22c46，v1.2.47）。
> 创建日期：2026-11-14。

## 一、差异总量（v1.0.11 → v1.2.47）

| 目录 | Mac 新增 | 同名文件改动 | 说明 |
|---|---|---|---|
| ui/src | 110 个文件 | 32/32 全部有差异 | Win 现有 32 个，Mac 现有 94 个 |
| server | 15 个新模块 | 21/21 有差异 | main.mjs：Win 1401 行 / Mac 2042 行 |
| shared | 33 个新模块 | — | Win 独有：appearance、screen-ambient（必须保留） |
| tests | 96 个新测试 | — | 同步时一并移植可移植部分 |
| docs | 大量功能文档 | — | SELECTION/POCKET/AMBIENT 等，随批次复制 |

Mac 端 Claude/Codex 旧面板已重构为统一 Agent 面板（`ClaudeCode.tsx`/`Codex.tsx` 在 Mac 端只剩 4 行存根）。

## 二、批次划分

每批完成后必须：`npm test` 全绿（现基线 363 项）、`cargo test` 16 项、`hidbridge --self-test`、UI 可视检查；并做一次 git 提交。

### B1 基础同步（无新 UI 入口，地基批）
- shared 新模块移植（33 个中与平台无关者先行）：`error-message`、`connection-display`、`device-status`、`preset-names`、`output-color-profile`、`permission-refresh`、`ui-overlays-i18n`、`connection-recovery`/`connection-wake` 相关的 `air-connection-wake`（平台部分换 Win 实现）等。
- server 通用能力：`connection-recovery.mjs`、`connection-wake.mjs`、`parent-lifetime.mjs`、`control-diagnostic.mjs`、`native-hold-env.mjs`、`native-mapping-env.mjs`（调用 HIDAridge 而非 Mac native pipe）、`native-key-auth.mjs`（Win 键钩子鉴权）、`sidebar-migration.mjs`。
- 同名 UI 文件差异同步：`api.ts`、`types.ts`、`App.tsx`、`Studio.tsx`、`Library.tsx`、`Device.tsx`、`i18n.ts` 等 32 个 —— 逐个三方比对，保留 Win 特有（screen-ambient 相关、start.cmd 集成）。
- 验收：现有 363 项测试全过 + Mac 对应新测试中平台无关项过。

### B2 功能中心 + 侧栏 + 设置页（承载后续所有功能的框架）
- `sidebar-features.mjs`、`FeatureCenter.tsx`、`feature-center.css`、`sidebar-migration`（升级保留已用入口）。
- `Settings.tsx`、`settings-overlays-v6.css`、About+Updates 合并布局、`StartupGuide.tsx`、`AudioPendingNotice.tsx`、`FeedbackToast.tsx`。
- 验收：侧栏可添加/移除功能入口；迁移逻辑测试过。

### B3 Agent 面板重构 + Codex 增强桥
- `AgentDashboard.tsx`、`AgentTabs.tsx`、`AgentQuota.tsx`、`AgentLighting.tsx`、`agent-v5.css` 等；Claude/Codex 旧面板替换为三页签（任务与快捷键/状态灯/接入设置）。
- server：`codex-command-bridge.mjs`、`codex-native-controls.mjs`、`codex-accessibility-bridge.mjs`（Win 用 UIA/键发送替代）、`codex-shortcut-setup.mjs`；`codex-shortcuts.mjs` 扩展（Win 116 行 → Mac 262 行）。
- agent-lighting 状态灯统一控制（1.2.8：对话键持续显示，其余键可播放作品）。
- 验收：Agent 三页签可用；快捷键录制与 Mac 快捷键表对齐。

### B4 Music 演奏模式 + 统一作品库
- ui：`src/music/` 全模块（MusicLab、LibraryView、engine、persistence、presets、面板图形）+ `ui/public/music/piano/`（31 mp3 + manifest + 许可）。
- server：`music-library.mjs`、`music-routes.mjs`、`music-import.mjs`；`shared/music-model.mjs`。
- `UnifiedLibrary.tsx` + `unified-library.css`（灯光+演奏合并、筛选、搜索、最近删除）。
- `.nuphy-music` 工程导入导出、WAV 导出。
- 验收：music-* 测试全过；资源完整；导入导出往返一致。

### B5 多键盘型号 + 编辑改进
- `shared/keyboard-models.mjs`、`additional-keyboards.json`、`kick75-ansi.json`、`keyboard-geometry.json/ts`、`air-key-geometry.json`、`layout`/`multi-layout`。
- server：`key-adaptation` 同步（Win 165 → Mac 202 行）、`hardware` 同步（289 → 427 行）、`firmware`（98 → 171 行，Kick75 旧固件引导 + firmware-onboarding）。
- 1.2.8 编辑改进：画板清空/撤销、逐帧动画只清当前帧、色盘改进、AI 状态灯预览不被亮度压暗。
- `KeyRecoveryPanel.tsx`、`FunctionKeyRecoveryDialog.tsx`、`keyboardPermissions.ts`。
- 验收：multi-layout / multi-model-service / key-adaptation-kick 测试过。

### B6 模拟声浪 Typing Sounds（需要 Win 原生键盘钩子）
- ui：`TypingSounds.tsx`、`typing-sounds.css`、音效资源（ui/public/typing-sounds/，含 ATTRIBUTION）。
- 原生：在 HIDBridge.exe（Rust）新增 WH_KEYBOARD_LL 全局键盘钩子 → 事件流（按键、按下/释放、无文字内容）供 server 播放音效；处理系统自动重复去重、修饰键过滤。
- server：`shared/typing-cross.mjs`（本应用听写/框选/麦克风律动期间暂停）、音量分级（空格/回车/退格/Shift/Tab/CapsLock）。
- 验收：typing-* 测试过；真机（无键盘则用软键盘）验证后台打字播放。

### B7 智能框选 + 拖拽转换 + 键盘口袋（Win 原生套件，最大批）
- 框选：Rust 壳新增 DXGI/GDI 屏幕捕获 + 全屏选区窗口（wry 透明窗口或原生绘制）；selection-kit 的 AIKit/FlowKit 逻辑移植（JS 层），NativeKit 换 Win 实现；AI 提供商配置（selection-providers.ts）。
- 拖拽转换：`DragConvertLab.tsx`、`shared/drag-trigger-keys.json`、拖拽触发键监听（复用 B6 钩子）。
- 键盘口袋：`KeyboardPocketLab.tsx`、`keyboard-pocket.css`；9 槽位收纳/取用（文字/图片/链接/文件/文件夹 ZIP）；Win 剪贴板读写（CF_TEXT/CF_UNICODETEXT/DIB/CF_HDROP）；自动粘贴用 SendInput Ctrl+V；顶部浮窗预览（替代 Mac 刘海展开）。
- server：`drag-host`、口袋持久化、与框选/Fn 冲突处理。
- 验收：selection/pocket/drag 测试过；无真机时用窗口模拟输入验证。

### B8 Win 更新器完整实现
- 更新器（Rust 壳内实现）：启动/定时检查 feed → 版本比较 → 下载 setup.exe（进度/校验 SHA-256 + ed25519 签名，对齐 Mac Sparkle 的签名理念）→ 校验通过后启动静默升级（Inno `/VERYSILENT /SUPPRESSMSGBOXES /RESTARTAPPLICATIONS`）→ 保留用户会话重启。
- feed：`updates/appcast.xml`（Win 版格式）+ `scripts/prepare-appcast.mjs` 适配 Win；`UpdatePreferences.tsx` + `update-notices.css` 设置页接入（B2 已铺 UI）。
- 版本信息接入 About 页；下载来源可配置（GitHub Releases 或自托管）。
- 验收：本地起静态 feed 模拟升级路径全流程（含签名篡改拒绝）。

### 明确不移植 / 替代
- Mac `ambient-lighting.mjs` 自动感光（macOS ALCALS 私有接口）：Windows 无通用光传感器 API，不移植；Win 已有 `screen-ambient.mjs`（屏幕氛围灯增强）继续保留。
- Sparkle 框架本身：由 B8 自研 Win 更新器替代。
- `fetch-sparkle.mjs`、`build-native.mjs`（Swift）：不适用。

## 三、平台适配总原则

1. 三进程架构不变：Rust 壳（wry/tao）→ Node server → HIDBridge.exe。
2. Mac 原生能力（Swift NativeKit）→ Win 对应实现：全局键盘钩子（HIDBridge）、屏幕捕获/选区/浮窗（Rust 壳）、剪贴板/自动粘贴（HIDBridge 或壳）。
3. 每引入一个 Mac 模块先检查其 import 面：native pipe 调用、Sparkle、ALCALS、辅助功能权限 → 换 Win 等价或条件禁用。
4. i18n：Mac 端 `ui-overlays-i18n.mjs`、`selection-messages.json`、music/messages.json 一并同步，中英双语对齐。
5. 资源文件（mp3、几何 json）直接从 `_mac_ref` 拷贝，许可文件（ATTRIBUTION/LICENSE）必须随附。
6. Win 独有资产白名单（任何同步不得覆盖）：`shared/screen-ambient.*`、`shared/appearance.*`、`native-win/`、`start.cmd`、Inno 打包脚本、`scripts/cargo-win.mjs`。
7. 版本策略：全部批次完成后 Win 版本升至 1.2.47 对齐（或按用户发布节奏另定）。

## 四、进度跟踪

| 批次 | 状态 | 提交 | 备注 |
|---|---|---|---|
| B1 基础同步 | 完成 | ebcc14d | shared 31 新模块 + 14 同名替换 + i18n 合并 + 8 server 模块 + fake-service（Win 5 处适配保留）|
| B2 功能中心/设置 | 完成 | 686c8bb + 5cf19c6 | UI 全量对齐（107 新文件 + 92 覆盖）+ App/AudioPanel/Settings 合并 Win 特有（appearance/screen）+ sidebarFeatures 持久化与迁移 + watchNativeParent 接线；npm test 393/381/0/12、UI 构建过 |
| B2 功能中心/设置 | 未开始 | — | |
| B3 Agent/Codex | 进行中（B3a 完成） | 0a3cfe4 | 4 个 codex 新模块 + codex-shortcuts（Win cmdorctrl→control 保留）+ codex.mjs 快捷键表演进（Win IS_WIN/路径清单保留）；claude 系列 Win 为超集零改动；npm test 406/394/0/12。B3c 待做：main.mjs 深度接线（codexAction 新流程/nativeKey 路由/CommandBridge/recovery/wake），恢复 audio-reactive skip 用例、codex-command-bridge-service.test、connection-* 测试 |
| B4 Music/统一库 | 完成 | 0b145e2 | server 随 B3c（music-routes/library/import + MusicRoutes 挂载）；12 个 music 测试 + source-ui-vm helper 恢复；parity 测试 CRLF 规范化 |
| B5 多键盘/编辑 | 完成 | 49c1c60 | key-adaptation/firmware 换 Mac 版；storage/engine/games canvasCount 化；main.mjs 移除过时 win32 adaptation gate；20 测试拷入；npm test 761/744/0/17 |
| B6 Typing Sounds | 完成 | feb459a | HIDBridge typing.rs（钩子/rodio/DSP/catalog 校验/6 op）；壳 shim 扩展 payload reply + /api/typing 转发；server 端点 + pause 协调；641 音效资源；cargo test 26 过、self-test 6115、npm test 766/749/0/17 |
| B7 框选/拖拽/口袋 | 完成 | b798ea7 + f2024c5 + a46aa8c | Pocket ~4200 行 16/16 op；DragConvert ~3900 行（IDropTarget 环形菜单、13 动作可用、pdfium/ffmpeg 降级）；Smart Selection server 1152 行（三协议 AI+SSE+key 隔离）+ 壳 2435 行（框选浮层/DXGI 区域截图/回答浮窗）。cargo test 119 过、npm test 784/767/0/17。降级：语音/OCR 未实现、Fn 不可检测、WebP 有损 |
| B7 框选/拖拽/口袋 | 未开始 | — | 最大批次，原生套件 |
| B8 更新器 | 进行中 | — | 方案：壳内 updater 模块（feed 解析 → ed25519-dalek+SHA-256 校验 → 下载 setup.exe → Inno /VERYSILENT /RESTARTAPPLICATIONS 静默升级）；scripts/prepare-appcast-win.mjs（node:crypto ed25519 签名，零依赖）；feed 默认 GitHub Releases latest；bridge appUpdate + About 页状态行；端到端本地模拟（校验篡改拒绝） |

## 五、B1 执行记录（2026-11-14）

1. **新模块拷贝（31 个）**：error-message、device-status、preset-names、output-color-profile、permission-refresh、sidebar-features、agent-lighting-view、keyboard-models、additional-keyboards.json、kick75-ansi.json、firmware-onboarding、typing-cross、music-model、air-connection-wake、ambient-lighting、connection-display（均含 .d.mts）+ selection-messages.json、ui-overlays-i18n.mjs。
2. **同名替换（13 个）**：keycap-functions、codex-keys(.d.mts)、lighting-power(.d.mts)、render-animation(.d.mts)、trigger(.d.mts)、presets(.d.mts)、layout.mjs —— 全部为 Mac 功能升级（多键盘型号、hue-shift、cross 灯效、定时器改进），无 Win 特有内容。
3. **i18n.mjs**：以 Mac 版为主体 + 追加 Win 独有 85 个 key（屏幕取色/外观/Win 模式/权限文案等）；删除 Mac 版重复 key "唤醒灯光"（保留后者 "Wake lights"，与运行时行为一致）；Win 端 npm 测试曾修复的缺失 key 已由 Mac 版覆盖。
4. **audio-reactive.mjs（手工合并）**：保留 Win screen source 扩展（AUDIO_SOURCES/DEFAULT_AUDIO.screen/校验分支）+ 合入 Mac 两处修复（sparkle 星星改按 layout 索引、`key.index>=83` → `key.controllable===false`）。
5. **测试**：presets/lighting-power/audio-reactive 三个测试换 Mac v1.2.47 版；fake-service.mjs 恢复 Win 版（Mac 版产生 ERR_UNSUPPORTED_ESM_URL_SCHEME，其 +235 行新能力待合并）；audio-reactive 集成用例 skip 待 B3。
6. **验证**：npm test 366 项 / 354 过 / 0 失败 / 12 跳过（11 平台 + 1 B3 待办）；对比基线 363/352/0/11。

**教训（合并脚本）**：i18n 合并脚本第一版用 `lastIndexOf('};')` 定位 messages 对象结尾，命中了 escape/patterns 代码段中的字符串，把 85 个 key 插进了函数体中间产生 SyntaxError；第二版改用 `indexOf('\n};')`（第一个独立闭合行）修复。文件合并后必须立即做一次 `import()` 冒烟验证。
