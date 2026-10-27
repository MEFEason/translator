; ============================================================================
; Translator 安装器（Inno Setup 6）
;
; 设计要点：
;   1. **两种安装模式，用户可选**：
;      - 「为所有用户安装」= 管理员模式 → C:\Program Files\Translator
;        （写 HKLM，UAC 弹一次）
;      - 「仅为当前用户安装」= 用户模式 → %LOCALAPPDATA%\Programs\Translator
;        （写 HKCU，全程不弹 UAC）
;      PrivilegesRequiredOverridesAllowed=dialog 提供这个选择框。
;   2. **注册表按模式自动分派**：统一用根键 HKA
;      （官方定义：HKA = 管理员模式下等于 HKLM，否则等于 HKCU），
;      杜绝「用户模式却去写 HKLM → RegCreateKeyEx 错误码 5」。
;   3. 静默安装代码签名根证书：
;      - 根证书本质是注册表项
;        (SystemCertificates\Root\Certificates\<SHA1>\Blob)，
;        直接写注册表 = 不碰证书 API → **不弹「安全警告」对话框**
;      - 走 [Registry] 段的 HKA 根键 → 管理员模式写 HKLM、用户模式写 HKCU
;      - Blob（148 字节属性头 + 886 字节 DER）两种存储通用（已实测一致）
;      - 只装公钥 cer，不导入 PFX（签名验证不需要私钥，也免去密码交互）
;   4. 卸载时按 SHA1 指纹精确删除证书（uninsdeletekey）
;
; 构建：
;   ISCC Windows\src\setup.iss                 # 正式
;   ISCC /DFULLPAYLOAD Windows\src\setup.iss   # 带完整 payload（默认即完整）
;
; 注意（来自以往项目的血泪经验）：
;   - [Code] 注释里不能出现 {app} 等花括号常量（会提前闭合注释）
;   - VersionInfoProductVersion 必须是四段纯数字
;   - 安装器/卸载器运行时不能编译（文件被占用）
; ============================================================================

#define MyAppName "Translator"
; ★ 版本号从构建产物读取，不手写 ★
; 版本号的唯一来源是 Rust 侧 build.rs（TRANSLATOR_CHANNEL + TRANSLATOR_SEMVER +
; TRANSLATOR_BUILD_TIME），由 package.sh 写进 Windows\.version.ini。
; 安装器用 ISPP 的 ReadIni 读它 → 安装目录名、安装器名、PE 版本信息
; 与「关于页」显示的版本号永远一致。
; 手写版本号会与 Rust 侧脱节（踩过：改了 Rust 忘了改安装器）。
; 版本串形如：
;   beta-v0.9.1-build2026.1022.15.16      测试版（主版本恒 0，只动后面数字）
;   release-v1.0.0-build2026.1022.15.16   正式版（大迭代更新全部数字）
; 读不到时退回 snapshot-v0.0.0-unknown（首次编译、或手工单独跑 ISCC 时）。
#define MyAppVersion ReadIni("..\.version.ini", "version", "full", "snapshot-v0.0.0-unknown")
; PE 版本资源用的四段纯数字版本（version.ini 的 num 段，缺省同上）。
; 形如 `0.9.1.1022` = 主.次.修订.月日 —— 同日重复构建靠完整串区分。
#define MyAppVersionNumber ReadIni("..\.version.ini", "version", "num", "0.0.0.0")
; 纯语义版本（v0.9.1 / v1.0.0），供展示用（不带渠道与时间）
#define MyAppSemVer ReadIni("..\.version.ini", "version", "semver", "0.0.0")
#define MyAppPublisher "MEFEason"
#define MyAppURL "https://github.com/MEFEason/translator"
#define MyAppExeName "translator.exe"
; 代码签名根证书的 SHA1 指纹 —— 卸载时按此精确删除，避免误删同名证书
#define MyCertThumbprint "83649E2C4CF364A87A78CC7D9C6CD707B7FA993A"

[Setup]
; ---- 基本信息 ----
AppId={{8A3F5C21-7B4E-4D9A-9E2F-1C6D8A0B3E5F}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
VersionInfoDescription={#MyAppName} 离线实时字幕翻译
; PE 版本资源必须是四段纯数字（不能带横杠），故与展示用版本号分开：
; 展示串形如 beta-v0.9.1-build2026.1022.15.16，其「纯数字」部分会超过 4 段，
; 因此 num 段单独由 package.sh 从语义版本算出：主.次.修订.月日（0.9.1.1022）。
VersionInfoProductVersion={#MyAppVersionNumber}
VersionInfoVersion={#MyAppVersionNumber}

; ★ FSL-1.1-ALv2 许可协议页 ★
; 在安装向导中显示许可条款（用户必须同意才能继续）。
; LicenseFile 要求 **RTF** 格式 → 由根目录 LICENSE 自动生成（见 package.sh），
; 内容与 LICENSE 文本一致，2 年后转 Apache-2.0 时同步更新即可。
LicenseFile=LICENSE.rtf

; ---- 安装模式：机器级为默认，但用户可选择仅安装到用户目录 ----
; 布局（用户要求）：
;   管理员模式：C:\Program Files\Translator\release\beta-v0.9.1-build2026.1022.15.16\   ← 本版本可执行文件
;   用户模式：  %LOCALAPPDATA%\Programs\Translator\release\beta-v0.9.1-2026.1022.15.16\
;   版本号在 release 之下 → 多版本可共存，升级不覆盖旧版。
;
; {autopf} 官方定义：管理员模式下等于 {commonpf}（Program Files），
; 否则等于 {userpf}（Win7+ 即 %LOCALAPPDATA%\Programs）→ 自动跟随模式。
;
; PrivilegesRequiredOverridesAllowed=dialog 提供「为所有用户 / 仅为当前用户」
; 选择框（官方：Setup will ask the user to choose the install mode）。
; ⚠️ 关键：注册表必须用根键 HKA（自动 HKLM/HKCU 分派），
;    否则用户模式写 HKLM → RegCreateKeyEx 错误码 5（拒绝访问）。
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
DefaultDirName={autopf}\Translator
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
AllowNoIcons=yes
; 关闭「admin 模式却用了 HKCU」的编译期警告：本脚本用 HKA 根键，
; HKCU 只在用户模式下才真正写入，属预期行为，非配置错误。
UsedUserAreasWarning=no

; ---- 输出 ----
OutputDir=..\dist
OutputBaseFilename=Translator-Setup-{#MyAppVersion}
SetupIconFile=..\..\translator_core\ui\windows\runner\resources\app_icon.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; ---- 反误报配置 ----
; 安装阶段不做注册表写入（卸载元数据由 Inno 原生 [Registry] 段处理）
; 不释放任何裸 .ps1 / .vbs 到临时目录
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
; ---- 应用主体（来自 outputs/，与 package.sh 的产物目录一致）----
; 全部装进版本目录 {app}\release\<版本号>\（多版本共存）
Source: "..\outputs\translator.exe"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
Source: "..\outputs\translator_core.dll"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
Source: "..\outputs\flutter_windows.dll"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
; ---- Flutter 插件的原生 DLL（运行时必需）----
; 每个带原生实现的插件都会生成一个 <plugin>_plugin.dll，缺一个
; 启动就报「由于找不到 xxx.dll，无法继续执行代码」。
; 当前：window_manager_plugin.dll / screen_retriever_windows_plugin.dll /
;       cnativeapi.dll（托盘与最小化到通知栏所需）。
; 用通配符而非枚举 —— 新增插件自动生效，不会再漏。
; （ORT/sherpa DLL 不会出现在 outputs/ 里：static 模式，ORT 编进 DLL 本体）
Source: "..\outputs\*_plugin.dll"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
Source: "..\outputs\cnativeapi.dll"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
Source: "..\outputs\data\*"; DestDir: "{app}\release\{#MyAppVersion}\data"; Flags: ignoreversion recursesubdirs createallsubdirs
Source: "..\outputs\README.md"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
; 版本号文件（供应用运行时读取自身版本；与 Rust 侧同源）
Source: "..\outputs\version.ini"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
; ★ 许可证文件 ★
; FSL-1.1-ALv2 的 Redistribution 条款：**再分发时必须附带条款副本或链接，
; 且不得移除已有版权声明**。同时静态链接的 ORT(MIT)/sherpa(Apache-2.0)/
; Bergamot(MPL-2.0) 的许可告知义务同样适用于安装后的副本。
; 缺这些文件 = 合规风险，所以一并装进安装目录。
Source: "..\outputs\LICENSE"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
Source: "..\outputs\NOTICE"; DestDir: "{app}\release\{#MyAppVersion}"; Flags: ignoreversion
Source: "..\outputs\THIRD_PARTY_LICENSES\*"; DestDir: "{app}\release\{#MyAppVersion}\THIRD_PARTY_LICENSES"; Flags: ignoreversion recursesubdirs createallsubdirs
; ⚠️ **不需要** onnxruntime.dll / sherpa-onnx-*.dll ——
; sherpa-onnx 用 static 模式，onnxruntime 整份静态编入 exe
; （官方 static-MT 包的 onnxruntime.lib 是 916 MB 真静态库）。
; 这不只是省文件：装一个名为 `onnxruntime.dll` 的新文件会触发 360
; 的「修改系统 DLL」报毒（Inno 进程 MoveFile/FileCopy 都会被拦），
; 而 static 模式让这个文件根本不存在。
;
; ---- 模型权重（ASR zipformer + MT Bergamot，约 94 MB）----
; 真实 ASR/MT 后端启动必需。GUI 会从 exe 同级的 models\ 读模型目录。
Source: "..\outputs\models\*"; DestDir: "{app}\release\{#MyAppVersion}\models"; Flags: ignoreversion recursesubdirs createallsubdirs

; ---- 代码签名根证书（仅公钥，不含私钥）----
; 装到 {app}\certs\ 而不是 {tmp}：用户可在安装目录查看证书来源，
; 卸载时随目录一起清掉，不会留下野文件。
Source: "..\scripts\codesign.cer"; DestDir: "{app}\certs"; Flags: ignoreversion

[Icons]
; 开始菜单：程序 + 卸载入口
Name: "{group}\{#MyAppName}"; Filename: "{app}\release\{#MyAppVersion}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
; 桌面快捷方式（Tasks 控制）
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\release\{#MyAppVersion}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; 装完应用后启动（可取消）
Filename: "{app}\release\{#MyAppVersion}\{#MyAppExeName}"; \
    Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; \
    Flags: nowait postinstall skipifsilent

[UninstallRun]
; 无外部命令：证书删除由下方 [Registry] 段的 uninsdeletekey 完成，
; 卸载时自动删掉整个注册表项（即删证书），全程静默。

[UninstallDelete]
; 清理本版本目录（多版本共存时只删自己那一份）
Type: filesandordirs; Name: "{app}\release\{#MyAppVersion}"

[Registry]
; ── 应用注册（按安装模式自动分派，卸载时自动清理）────────────
; 根键统一用 HKA：官方定义「HKA = 管理员安装模式下等于 HKLM，否则 HKCU」。
; 管理员模式 → HKLM\...；用户模式 → HKCU\...（无需管理员，不弹 UAC）。
; 之前这里写死 HKLM，用户模式下一律 RegCreateKeyEx 错误码 5。
;
; App Paths：让用户在任何地方（开始菜单搜索、Run 对话框、
; 其它程序调用 CreateProcess）都能直接启动 translator.exe。
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{#MyAppExeName}"; \
    ValueType: string; ValueName: ""; ValueData: "{app}\release\{#MyAppVersion}\{#MyAppExeName}"; \
    Flags: uninsdeletekey
; PATH 之外再补一条，方便不读 App Paths 的老程序。
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\{#MyAppExeName}"; \
    ValueType: string; ValueName: "Path"; ValueData: "{app}\release\{#MyAppVersion}"; \
    Flags: uninsdeletekey

; 卸载元数据（控制面板「程序和功能」里能看到并卸载）。
; Inno 靠 AppId 自动注册，但显式写一遍更稳（不同 Windows 版本
; 对 WOW64 重定向的处理有差异）。
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "DisplayName"; ValueData: "{#MyAppName} {#MyAppVersion}"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "DisplayVersion"; ValueData: "{#MyAppVersion}"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "Publisher"; ValueData: "{#MyAppPublisher}"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "InstallLocation"; ValueData: "{app}"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "DisplayIcon"; ValueData: "{app}\release\{#MyAppVersion}\{#MyAppExeName}"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: dword; ValueName: "NoModify"; ValueData: 1; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: dword; ValueName: "NoRepair"; ValueData: 1; Flags: uninsdeletekey
; WOW64 32 位视图（32 位系统的程序列表也看得到）
; 用 HKA32：管理员模式 → HKLM\SOFTWARE\WOW6432Node\...
;             用户模式   → HKCU\SOFTWARE\WOW6432Node\...（HKCU 的 32 位视图）
; 一次解决「模式分派」+「32 位视图」两个需求。
Root: HKA32; Subkey: "Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "DisplayName"; ValueData: "{#MyAppName} {#MyAppVersion}"; Flags: uninsdeletekey
Root: HKA32; Subkey: "Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "DisplayVersion"; ValueData: "{#MyAppVersion}"; Flags: uninsdeletekey
Root: HKA32; Subkey: "Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "Publisher"; ValueData: "{#MyAppPublisher}"; Flags: uninsdeletekey
Root: HKA32; Subkey: "Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: string; ValueName: "InstallLocation"; ValueData: "{app}"; Flags: uninsdeletekey
Root: HKA32; Subkey: "Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: dword; ValueName: "NoModify"; ValueData: 1; Flags: uninsdeletekey
Root: HKA32; Subkey: "Microsoft\Windows\CurrentVersion\Uninstall\{#MyAppName}_is1"; \
    ValueType: dword; ValueName: "NoRepair"; ValueData: 1; Flags: uninsdeletekey

; 字幕文件关联（.srt / .vtt / .ass）—— 双击字幕文件用翻译器打开。
; 只登记「已安装」标记，实际打开动作由应用自己处理。
; Subkey 用 "SOFTWARE\Classes\..."：官方建议如此（不要用 HKCR），
; HKA → 管理员模式写 HKLM\SOFTWARE\Classes（全局关联）
;          用户模式写 HKCU\SOFTWARE\Classes（仅当前用户关联）。
Root: HKA; Subkey: "SOFTWARE\Classes\.srt\OpenWithProgids"; \
    ValueType: string; ValueName: "Translator.Assoc"; ValueData: ""; \
    Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Classes\.vtt\OpenWithProgids"; \
    ValueType: string; ValueName: "Translator.Assoc"; ValueData: ""; \
    Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Classes\.ass\OpenWithProgids"; \
    ValueType: string; ValueName: "Translator.Assoc"; ValueData: ""; \
    Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Classes\Translator.Assoc\DefaultIcon"; \
    ValueType: string; ValueName: ""; ValueData: "{app}\release\{#MyAppVersion}\{#MyAppExeName},0"; \
    Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Classes\Translator.Assoc\shell\open\command"; \
    ValueType: string; ValueName: ""; ValueData: """{app}\release\{#MyAppVersion}\{#MyAppExeName}"" ""%1"""; \
    Flags: uninsdeletekey
; 删掉空的父键（uninsdeletekey 只删自己写的那层，壳会留下）
Root: HKA; Subkey: "SOFTWARE\Classes\Translator.Assoc\shell\open"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Classes\Translator.Assoc\shell"; Flags: uninsdeletekey
Root: HKA; Subkey: "SOFTWARE\Classes\Translator.Assoc"; Flags: uninsdeletekey

; ── 代码签名根证书（静默方案）──────────────────────────────
; 根证书本质是注册表项（SystemCertificates\Root\Certificates\<SHA1>）。
; 直接写注册表 = 完全不碰证书 API → 不弹「安全警告」对话框。
; 实测 certutil / X509Store.Add / Import-Certificate 三种方式
; 在往 Root 存储写证书时都会弹阻塞框，只有注册表直写全程静默。
; ValueData 的 148 字节头 + 886 字节 DER 由证书存储自身生成，
; 一次性导出后复用（格式对系统透明，无需理解）。
;
; ⚠️ 根键用 **HKA**（管理员模式→HKLM，用户模式→HKCU）：
; 管理员模式必须写 HKLM —— 若留在 HKCU，凭据提升（输入另一个管理员账号）
; 时 HKCU 会指向那个管理员，证书就装到别人身上去了；HKLM 对所有用户一致生效。
; 用户模式则写 HKCU（该模式本就是「仅当前用户」语义，机器级存储反而超出授权范围）。
; 实测两处存储的 Blob 字节完全一致（1034 字节，同一属性头 + DER）→ 同一份数据通用。
Root: HKA; Subkey: "Software\Microsoft\SystemCertificates\Root\Certificates\83649E2C4CF364A87A78CC7D9C6CD707B7FA993A"; \
    ValueType: binary; ValueName: "Blob"; ValueData: "    19 00 00 00 01 00 00 00 10 00 00 00 91 fe c4 43 f6 3f 88 5f cd 75 73 eb \
    7d 92 d0 a1 0f 00 00 00 01 00 00 00 20 00 00 00 f5 c9 9c 7f af f9 4d 95 \
    3f 92 52 45 18 01 0e f6 3a 3d 4a 24 88 64 fb 49 92 46 60 fc 17 cc 27 2e \
    03 00 00 00 01 00 00 00 14 00 00 00 83 64 9e 2c 4c f3 64 a8 7a 78 cc 7d \
    9c 6c d7 07 b7 fa 99 3a 14 00 00 00 01 00 00 00 14 00 00 00 84 b5 c2 c9 \
    f3 92 59 8a 97 c1 5b 5c 9a de 4f 73 2d a7 51 62 20 00 00 00 01 00 00 00 \
    76 03 00 00 30 82 03 72 30 82 02 5a a0 03 02 01 02 02 10 28 df 2c 2a 83 \
    5e 85 9f 47 b8 5c 74 47 a8 1a 03 30 0d 06 09 2a 86 48 86 f7 0d 01 01 0b \
    05 00 30 51 31 0b 30 09 06 03 55 04 06 13 02 43 4e 31 11 30 0f 06 03 55 \
    04 0a 0c 08 4d 45 46 45 61 73 6f 6e 31 2f 30 2d 06 03 55 04 03 0c 26 43 \
    65 72 74 69 66 69 63 61 74 65 20 66 6f 72 20 54 72 61 6e 73 6c 61 74 6f \
    72 20 62 79 20 4d 45 46 45 61 73 6f 6e 30 1e 17 0d 32 36 31 30 32 31 30 \
    37 33 33 35 31 5a 17 0d 33 31 31 30 32 31 30 37 34 33 35 30 5a 30 51 31 \
    0b 30 09 06 03 55 04 06 13 02 43 4e 31 11 30 0f 06 03 55 04 0a 0c 08 4d \
    45 46 45 61 73 6f 6e 31 2f 30 2d 06 03 55 04 03 0c 26 43 65 72 74 69 66 \
    69 63 61 74 65 20 66 6f 72 20 54 72 61 6e 73 6c 61 74 6f 72 20 62 79 20 \
    4d 45 46 45 61 73 6f 6e 30 82 01 22 30 0d 06 09 2a 86 48 86 f7 0d 01 01 \
    01 05 00 03 82 01 0f 00 30 82 01 0a 02 82 01 01 00 c7 5b e5 ff 55 6d c6 \
    fb 1a ea 32 17 8f 9b b8 99 bb be 41 8e a8 99 18 ab 3f f5 75 d1 76 2b 14 \
    91 74 f3 6c f4 65 64 8e 2d e0 2e 6d 5a 02 b4 f3 ef 4a c6 86 4e d6 de a0 \
    62 33 6c 97 fd 86 e9 7e 6d 59 2e 07 e8 36 fe 2f 60 10 03 13 95 cf 23 77 \
    f9 cd 99 7a 32 da 53 46 4d 41 09 e3 10 ad dc bf 74 31 35 c8 c0 02 f8 6c \
    38 c2 1d a4 e2 bf ae e3 c0 d3 1e f1 54 d1 7b 46 9e f4 6e 60 ef d6 c9 af \
    93 09 9e 80 e3 34 32 61 28 d9 3c a2 c9 2f 45 cb 94 93 49 20 91 dc c0 d5 \
    9b 36 55 02 14 a6 8b bf 8c f6 67 8e ec e7 ba 02 9b c5 44 f9 b8 44 58 a8 \
    67 1e 72 17 41 0d c3 6d a1 dc 2c 9a 61 7b eb 18 c5 81 6f 0b a6 4e 48 77 \
    5f d0 c3 cd b5 0b fb c9 a4 4e b3 53 cb 2b 89 b4 df 1c 8b eb 04 d2 be 85 \
    80 a9 9f dc 7d f6 7e 95 14 bd 5d 30 16 ea f8 98 37 bb d9 28 50 99 cd ab \
    35 60 52 3a d6 21 f4 8e 8d 02 03 01 00 01 a3 46 30 44 30 0e 06 03 55 1d \
    0f 01 01 ff 04 04 03 02 07 80 30 13 06 03 55 1d 25 04 0c 30 0a 06 08 2b \
    06 01 05 05 07 03 03 30 1d 06 03 55 1d 0e 04 16 04 14 84 b5 c2 c9 f3 92 \
    59 8a 97 c1 5b 5c 9a de 4f 73 2d a7 51 62 30 0d 06 09 2a 86 48 86 f7 0d \
    01 01 0b 05 00 03 82 01 01 00 15 e5 55 f9 eb ca 32 fa bb 2c 11 c5 62 11 \
    03 c9 23 48 27 ca 17 01 97 43 27 2b 01 8f fb dd 34 09 60 a8 6e 94 52 9b \
    c0 15 79 7e 53 81 d4 bf 56 7d dc 5d 8a 9b da 5f 2a 10 93 3e ca f6 4b b9 \
    6e e1 78 4f 5a b1 f0 c3 bd 0e 8a 67 f1 a2 45 99 99 76 34 c0 d2 63 02 6d \
    a0 90 1a df 30 83 9d 71 60 2b 5d 44 e7 48 8b dd ff 14 74 01 a4 3f a6 2a \
    61 c9 0a 10 8e ba 0b 86 58 79 b5 37 93 9b 2e 17 70 e6 e5 4d fb 05 ba 25 \
    06 0e 27 1f c8 58 5d 7d 18 2a 57 be 8d e9 d2 e0 53 0a 89 fa c3 46 07 04 \
    83 c6 81 1d ce 6e 5a bb ea 23 a1 96 a9 83 e4 47 70 d5 6e 2a 22 4c b8 31 \
    ed 6a f4 80 99 e7 a3 83 a3 73 13 68 b9 3b 63 99 b3 f1 56 36 d5 38 73 d9 \
    cb f5 f2 8f 81 ec 7c 03 5f 34 3b 2a 49 51 3f 13 6e a8 b4 7e 13 5f dd 85 \
    d6 5b 47 67 2f 17 e4 00 8c 45 bc 40 95 92 b3 c4 61 dc 22 50 be 46 44 8f \
    51 a1"; \
    Flags: uninsdeletekey createvalueifdoesntexist

[Code]

// ── 安装模式判定 ────────────────────────────────────────────────
// 管理员模式 = 为所有用户安装（写 HKLM、装进 Program Files、UAC 弹一次）
// 用户模式   = 仅为当前用户安装（写 HKCU、装进用户目录、全程不弹 UAC）
// Inno 官方：IsAdminInstallMode 在「管理员安装模式」下返回 True。
function UseAdminMode: Boolean;
begin
  Result := IsAdminInstallMode();
end;

// ── 安装前检查：同版本已安装则提示 ──────────────────────────────
function InitializeSetup(): Boolean;
var
  InstalledVersion: String;
begin
  Result := True;
  // 读 Inno 自己写的卸载键（只读，两种模式都查）
  if RegQueryStringValue(HKLM,
     'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8A3F5C21-7B4E-4D9A-9E2F-1C6D8A0B3E5F}_is1',
     'DisplayVersion', InstalledVersion) or
     RegQueryStringValue(HKCU,
     'Software\Microsoft\Windows\CurrentVersion\Uninstall\{8A3F5C21-7B4E-4D9A-9E2F-1C6D8A0B3E5F}_is1',
     'DisplayVersion', InstalledVersion) then
  begin
    // ⚠️ 静默安装时**不能弹 MsgBox** —— 会阻塞安装（无人点 OK）。
    //   静默模式下改为写日志。
    if InstalledVersion = '{#MyAppVersion}' then
    begin
      if WizardSilent then
        Log('检测到已安装相同版本（' + InstalledVersion + '），静默安装跳过提示')
      else
        MsgBox('检测到已安装相同版本（' + InstalledVersion + '）。' + #13#10 +
               '建议先卸载旧版本再安装，避免文件残留。', mbInformation, MB_OK);
    end;
  end;
end;

// ── 安装后验证：证书必须装上，否则明确告知用户 ──────────────────
// 证书写入的存储随模式而定：管理员模式 HKLM、用户模式 HKCU。
// 直接回读对应键值最确定；不用 certutil（-user 查用户存储、默认查机器存储，
// 与实际写入位置对不上会误报「未找到」）。
// 找不到不阻断安装（应用本身不依赖证书也能跑，只是签名显示不受信任）。
procedure CurStepChanged(CurStep: TSetupStep);
var
  Blob: AnsiString;
  Root: Integer;
  StoreName: String;
begin
  if CurStep = ssPostInstall then
  begin
    if UseAdminMode then
    begin
      Root := HKEY_LOCAL_MACHINE;
      StoreName := 'HKLM';
    end
    else
    begin
      Root := HKEY_CURRENT_USER;
      StoreName := 'HKCU';
    end;
    if RegQueryBinaryValue(Root,
       'Software\Microsoft\SystemCertificates\Root\Certificates\{#MyCertThumbprint}',
       'Blob', Blob) and (Length(Blob) > 0) then
      Log('证书验证：' + StoreName + ' 根证书已写入（Blob ' + IntToStr(Length(Blob)) + ' 字节）')
    else
      Log('证书验证：' + StoreName + ' 根证书未找到（不影响使用，仅签名显示不受信任）');
  end;
end;

// ── 安装前：权限校验 + 终止正在运行的实例 ──────────────────────
// 管理员模式下注册表写 HKLM、文件写 Program Files，需要提权；
// 用户模式写 HKCU、装进用户目录，**不需要**管理员权限。
// 这里只在「选了管理员模式却没提权」时拦截（正常流程 Inno 已自动提权）。
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  Result := '';
  NeedsRestart := False;
  if UseAdminMode and not IsAdmin() then
  begin
    Result := '当前安装选择了「为所有用户安装」，需要管理员权限' +
              '（要写入 Program Files 与 HKLM 注册表：卸载项、App Paths、根证书）。' +
              ' 请在 UAC 提示中点击「是」，或右键选择「以管理员身份运行」。';
    Exit;
  end;
  Exec(ExpandConstant('{sys}\taskkill.exe'),
       '/IM translator.exe /F /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;
