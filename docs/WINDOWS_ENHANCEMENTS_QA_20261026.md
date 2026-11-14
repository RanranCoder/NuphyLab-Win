# Windows 增强批次(自启/睡眠/系统主题/屏幕氛围光)QA 记录 · 2026-10-26

环境:Windows 11 10.0.26300 · rustc windows-gnu · 系统主题为深色(AppsUseLightTheme=0)· 主显示器 3840×2160 · 隔离数据目录。本机无键盘,涉及硬件输出的项(屏幕取色实际点灯、睡眠恢复后采集重启)列入联调清单;其余全部实测。

## ① 登录自启(HKCU Run)

- 实现:`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `NuPhyLab` 值,内容为带引号的绝对路径(安装路径常含空格)。托盘菜单新增「开机自动启动」勾选项(CheckMenuItem),点击即翻转并回写菜单;失败弹 MessageBoxW 并保留原勾选状态。启动时读注册表决定初始勾选;若值指向旧安装位置则自动改写成当前 exe(便携包换目录后自愈)。
- 匹配语义 `run_value_matches`:去引号、trim、大小写不敏感;带参数(`"…exe" --minimized`)不算匹配。Rust 单测覆盖 5 种形态。
- **实测**(`AIR_STUDIO_AUTOSTART_QA=1` 自检入口,与托盘同一代码路径):
  ```
  before: false
  after enable: stored="D:\…\target\release\NuPhyLab.exe" enabled=true
  restored: false
  ```
  启用后 `Get-ItemProperty` 读到带引号路径;自检结束恢复原状态,注册表值被 `RegDeleteValueW` 清空——无残留。
- 限制:删除不存在的值(错误码 2)视为成功(幂等);不写 HKLM(per-user 安装无需提权)。

## ② 睡眠/唤醒(WM_POWERBROADCAST)

- 实现:独立线程注册 message-only 窗口(hwndParent = HWND_MESSAGE),消息泵接收 `WM_POWERBROADCAST`;`PBT_APMSUSPEND` → 停音频与屏幕采集;`PBT_APMRESUME` / `PBT_APMRESUMEAUTOMATIC` → 记日志,由 0.5 s 的 /api/state 轮询按需重启采集(与 macOS `NSWorkspace.willSleepNotification` 的 `audioCapture.stop()` 对齐)。休眠从不 veto(返回 TRUE = 已处理)。
- 窗口类注册失败时静默返回:会话代际看门狗与 poll 重启路径仍能覆盖后果(与 M3 记录的"系统待机唤醒未接入"限制相比,这是补齐而非替代)。
- **未实测**:真实睡眠/唤醒(需合盖或手动挂起,避免打断当前会话);列联调项——休眠时 worker.log 应出现 `[power] suspending: stopping audio and screen capture`,唤醒后出现 `[power] resumed` 且采集自动恢复。

## ③ 主题:应用内外观切换 + 系统深浅色/强调色

- 实现(分两层,职责不重叠):
  - **壳只发布系统事实**:读 `HKCU\…\Themes\Personalize\AppsUseLightTheme`(0 = 深色)与 `HKCU\…\DWM\ColorizationColor`(0xAABBGGRR → `#RRGGBB`);4 s 轮询线程,仅变化时推送。注入脚本设 `document.documentElement.dataset.osThemeSystem`、CSS 变量 `--accent` 并派发 `nuphylab:os-theme` 事件——**壳不再决定最终主题**(否则每 4 s 会把手动选择覆盖回去)。
  - **UI 决定最终主题**:`shared/appearance.mjs` 提供 `light | dark | system`,默认 **`light`**;`useAppearance` 在 App 挂载时与 `nuphylab:os-theme` 事件里计算 `effectiveTheme(appearance, systemDark)` 并写 `dataset.osTheme`。未知值一律回落浅色(不会因深色桌面意外变深)。
- 切换入口:侧栏底部「外观 / Appearance」下拉(语言选择器下方),三选项浅色/深色/跟随系统;值存 `settings.appearance`,经 `/api/state` 持久化,重启保持。
- CSS:`:root[data-os-theme="dark"]` 覆盖 `--ink/--muted/--line/--surface/--panel/--wash/--shadow` 与 `body` 背景;主按钮反相(浅底深字)、侧栏选中态、音频源/效果按钮选中态、输入控件、`color-scheme: dark`。`styles.css` 16 处与 `studio-design.css` 5 处字面白底改为 `var(--panel)` / `var(--wash)`,主题统一生效(macOS 行为不变,浅色下取值与原字面量相同)。聚焦环用 `--accent`(系统强调色)。
- **实测**(QA 探针含 `osTheme`/`osThemeSystem`/`darkPanel`/`appearanceSelect`;本机系统为深色):
  ```
  默认(新库):      {"osTheme":"light","osThemeSystem":"dark","darkPanel":"#fff","appearanceSelect":"light"}
  appearance=system: {"osTheme":"dark","osThemeSystem":"dark","darkPanel":"#1c1c1b","appearanceSelect":"system"}
  ```
  第一轮证明**默认浅色压过深色系统**;第二轮证明「跟随系统」在深色桌面下正确变深,且选择器读出了持久化值。Rust 单测覆盖 ABGR 转换与「注入脚本只写系统值、不写最终主题」;JS 单测覆盖默认值、非法值拒绝与 `effectiveTheme` 四种组合。
- 限制:暗色只反转界面外壳;键帽绘制、品牌色(sage/coral)、色轮保持原样(刻意保留产品识别)。切主题无需重载页面(纯 CSS 变量)。启动首帧:壳在 WebView 建好后立即注入系统值,但 `appearance=dark` 时页面仍可能先以浅色绘制一瞬(设置存在 library.json,页面脚本执行前拿不到),属可接受的加载闪烁;如需消除,需让壳在初始化脚本里预置 `appearance`(涉及 shim 结构,暂未做)。


## ④ 屏幕氛围光(DXGI Desktop Duplication)

- 实现:`capture.rs` 复制主显示器 → staging 纹理 → Map → 6×16 区域平均(与键盘 6 行 × 16 列几何精确对应,单测断言);仅区域平均值离开 native 层,像素不落盘、不上传。`screenPacket`(opus/status/rows/cols/zones)复用音频会话生命周期:sessions、代际计数器、45 ms 节流、1 s 静默保活、`IDXGI_ERROR_ACCESS_LOST` 自动重建、丢失/失败上报 `screenCapture`。
- **踩坑与修复(实测抓出)**:初版用 `DuplicateOutput` + 固定 BGRA8 staging,结果 **431 帧全黑**。`CopyResource` 要求源/目标格式一致,而 4K 桌面在 HDR/10-bit 配置下 duplication 的原生 surface 并非 BGRA8,复制静默失败。改用 `IDXGIOutput5::DuplicateOutput1` 显式请求 `DXGI_FORMAT_B8G8R8A8_UNORM`(Win10 1703+),旧输出回退 `DuplicateOutput`。修后同一探针:源格式 `DXGI_FORMAT(87)`(= BGRA8)、`copy failures: 0`、区域色阶 `#151515..#3e3e3e`、spread 63 —— 真实屏幕内容。
- **竖屏隐患**:staging 尺寸原先取桌面 rect,旋转显示器下与 surface 尺寸不符(又会静默黑屏)。改为取 `duplication.GetDesc().ModeDesc` 的宽高。
- 渲染(shared/screen-ambient.mjs):三种效果——屏幕映射(每键取所在网格)、整体色调、边缘辉光(外圈跟随四边、中心衰减);`gain` 0.5–2 亮度增强(先减底噪再乘,避免灰屏泛白)、`smoothing` 0–0.95 时间常数(快起慢落,同音频包络)。数据缺失/过期时渐隐而非冻结。84 号 LED 非按键,保持熄灭(与音频一致)。
- server 接入:源枚举扩为 `system|microphone|screen`;`screenPacket` op → `AudioSession.ingest` 分派;音频包误投到屏幕会话(或反之)显式报错;`audioStart` 在非 win32 拒绝 screen 源(UI 之外的第二道门);`inventory` 新增 `screenSupported`(壳恒报 true)。
- UI:声音页新增第三个源按钮「屏幕颜色」(仅 `screenSupported` 时出现);切换后标题变「屏幕取色」,效果选择器换 3 项,颜色区改为说明,参数区换「亮度增强 / 颜色过渡」,电平条改为平均色色块;文案与页脚统一说明"不截屏、不保存、不上传"。
- **实测**:探针 2 s → 290 帧、0 拷贝失败、区域色有层次;server 侧单测覆盖包校验(6×16、通道 0–255、整数)、三种效果、平滑/渐隐、会话路由与错误路径。**未实测**:真实键盘点灯效果(需硬件)、录制软件独占时的失败提示。

## 测试

- Rust:`cargo test -p shell` = 10 项全过(新增 capture 3、autostart 1、theme 2);`cargo build --release` 零警告;`hidbridge --self-test` 5353 项通过。
- JS:`npm test` 355 项 → 344 过 / 0 失败 / 11 平台跳过。新增 `tests/screen-ambient.test.mjs` 12 项、`tests/appearance.test.mjs` 3 项;`tests/storage.test.mjs` 适配 audio 设置新增的 `screen` 子对象。
- 壳冒烟(QA 模式,隔离数据目录):ExitCode 0,探针输出含新的主题字段。
- 打包布局不变(M5 链路未受影响);`docs/` 不随包分发,本次新增文档仅仓库内。

## 联调清单(需键盘/真实电源事件)

1. 屏幕取色实际点灯:三种效果各跑一轮,视频/图片/桌面切换时观察跟随与延迟(45 ms 周期 + smoothing)。
2. 屏幕源与 USB 断开的交互:播放中拔线应停采并报 `keyboardDisconnected`。
3. 睡眠/唤醒:合盖 10 s 后唤醒,确认日志两条记录与采集自动恢复。
4. 深色/浅色在侧栏「外观」下拉中切换后立即生效(不重载页面);选「跟随系统」时改 Windows 设置里的模式,4 s 内跟随;强调色改变后同样 4 s 内跟随。
5. 托盘「开机自动启动」勾选状态与注册表一致性(手动切换后重启应用仍保持)。
6. 与录屏软件共存:被独占时 UI 应显示 `screenCapture` 错误而非黑屏。
