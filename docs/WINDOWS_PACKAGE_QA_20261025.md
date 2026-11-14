# Windows 打包(M5)QA 记录 · 2026-10-25

环境与 M2–M4 记录相同(Windows 11 · rustc windows-gnu · node 26.12 PATH)。本机无 signtool(无 Windows SDK)与 Inno Setup 6,签名与安装包走代码路径 + 单测覆盖,真实签名/安装验证列入联调清单。

## 构建链

- `npm run build:icon:win`(png2icons 2.0.1,根 devDependency):`logo/头像.png` → `assets/AppIcon.ico`,9 档(16–256px)生成,432KB。
- exe 图标/版本信息嵌入:shell 新增 `build.rs` + winresource 0.1(GNU 工具链走 windres,cargo-win.mjs 的 mingw binutils 已含)。版本读根 `package.json`,VERSIONINFO 四段式(1.0.11 → 1.0.11.0,打包值 MAJOR<<48|MINOR<<32|PATCH<<16)。实测 `Get-Item VersionInfo` 全字段正确、`ExtractAssociatedIcon` 32×32 非空;图标缺失时 warning 降级不阻塞开发构建。构建警告清零(顺手把 M3 遗留的 `event` 句柄改名 `_event`)。
- HIDBridge `--self-test` 5353 项通过;`release/win/NuPhyLab-win-x64/runtime/node.exe --version` = v24.21.0。

## fetch-node(win-x64)

- 官方 win-x64 zip 实际下载成功:SHASUMS256 校验 + PE 头校验(MZ / PE 签名 / 0x8664 机器类型),SHA-256 记录于构建输出;解压用 Windows 自带 bsdtar(`System32\tar.exe`),失败回退 PowerShell Expand-Archive。darwin 分支代码路径不变(otool 逻辑保留)。

## package-win(真实打包一轮)

- `AIR_STUDIO_ALLOW_UNSIGNED=1 node scripts/package-win.mjs` 全流程通过:布局组装(根布局 → `release/win/NuPhyLab-win-x64/`)→ signing.json(未签名如实标记 `adhoc:true`)→ 布局完整性校验(LAYOUT_SPEC 9 项,file 空文件/dir 空目录/缺失分列)→ 打包内 node.exe 启动校验 → Compress-Archive 便携 zip(38.8MB)。ISCC 未安装,安装包优雅跳过并提示。
- 打包布局壳冒烟(隔离数据目录 + `AIR_STUDIO_SHELL_QA=1`):ExitCode 0——exe 旁资源解析、bundled runtime/node.exe 启动、ready 握手、自动退出链在**打包布局**下全部工作(此前 M2 冒烟验证的是仓库根布局)。

## 签名(signing-win)

- 纯逻辑单测 5 项:signtool 发现(显式 > PATH > Windows Kits 按版本号逐段降序 + x64 优先;实现在测试驱动下抓出并修复了 `Number('10.0.22621.0')` = NaN 的排序 bug)、证书选择(唯一自动选/指定指纹或主题/缺失与多义报错)、签名参数(PFX /f /p 与 /sha1 两形;时间戳开关)、signAll 签名→验证顺序与 unsigned 零调用、signingHandle 降级门(无 signtool 必须显式 `AIR_STUDIO_ALLOW_UNSIGNED=1`;PFX 跳过存储枚举;枚举命令为只读,单测断言无写操作)。
- 实现期还修复一处会导致挂起的 promisified-exec 误用(where.exe 失败路径的 promise rejection 未消费)。
- 真实签名未验证(本机无证书/SDK):列联调清单——证书安装、自动选择、签名 + verify 全绿、签名后 SmartScreen 行为、杀软扫描。

## 安装包(installer-win.iss)

- 脚本就绪:per-user 安装(`{localappdata}\Programs\NuPhyLab`,无 UAC,HID/钩子无需管理员)、稳定 AppId GUID、lzma2 压缩、`PrepareToInstall`/`CurUninstallStepChanged` taskkill NuPhyLab.exe + HIDBridge.exe(Job Object 级联收子进程;明确不碰通用 node.exe)、可选桌面图标、安装后启动。`/Qp` 静默构建由 package-win 驱动。
- 未实测:本机无 Inno Setup 6。ISCC 检测(显式 env → Program Files (x86) → Program Files)有单测;装好 ISCC 后重跑 `package:win` 即可出 setup.exe。

## WebView2 缺失引导

- main.rs webview 创建失败分支:错误串含 webview2 / 0x80070002 / cannot find the file 时,改为 ask_yes_no 引导打开 Evergreen 下载页(Win10 无内置运行时的场景);其余错误维持原 alert。该路径在装有 WebView2 的机器上不可达,未实机触发,列联调清单(可用注册表删除运行时模拟,或 Win10 干净虚拟机)。

## 测试结果

- 全套 `npm test`:340 项,329 过,0 失败,11 平台跳过。新增 signing-win 5 项、packaging-win 3 项(布局校验/LAYOUT_SPEC 覆盖/ISCC 发现)、packaging-runtime 增 2 项(archive 矩阵、PE 头校验含截断/坏 MZ/机器类型拒绝)并适配 win 的 `node.exe` vendor 路径断言。

## 已知限制与联调清单(M5 验收项)

1. Authenticode 签名全链路(证书安装 → 自动选择 → sign + verify → 签名后包在干净 VM 的 SmartScreen/Defender 行为)。
2. Inno Setup 安装包构建 + 干净虚拟机(Win10 21H2 / Win11)安装 → 全功能 → 升级覆盖 → 卸载,无残留进程/文件。
3. WebView2 缺失引导实机(Win10 无运行时虚拟机)。
4. 便携 zip 在含空格/中文路径下解压运行。
5. signing.json `adhoc:true` 的未签名包不得对外分发(文档已写明)。
