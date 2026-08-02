; SteamTools Inno Setup 安装器
;
; 构建: iscc /DVERSION=0.1.0 installer/steamtools.iss
; 安装器把 3 个 DLL 放进 Steam 根目录, 注册卸载信息; 卸载时删文件与数据目录.

#ifndef VERSION
  #define VERSION "0.0.0"
#endif

#define APP_NAME "SteamTools"
#define APP_PUBLISHER "SteamTools Project"
#define APP_URL "https://github.com/SteamToolsProject/SteamTools"

[Setup]
AppId={{8A3E6C11-3D1E-4F9A-9B4C-2D0F6E5A7B1C}
AppName={#APP_NAME}
AppVersion={#VERSION}
AppPublisher={#APP_PUBLISHER}
AppPublisherURL={#APP_URL}
AppSupportURL={#APP_URL}
DefaultDirName={code:GetSteamDir}
; 安装位置定死为注册表里的 Steam 根目录, 不让用户改.
DisableDirPage=yes
; 卸载器收敛进数据目录, 别污染 Steam 根目录.
; 不放 {app}\steamtools\update: 那是自更新工作区, 版本追平后会被整体清理.
UninstallFilesDir={app}\steamtools
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\dist
OutputBaseFilename=SteamTools-Setup-{#VERSION}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#APP_NAME} {#VERSION}
UninstallDisplayIcon={app}\stbase.dll
; 安装器本身不改系统: 只往 Steam 目录写文件, 不需要 admin (Steam 装在
; Program Files 时由 PrivilegesRequiredOverridesAllowed 提示提权).

[Languages]
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "..\target\release\stbase.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\dwmapi.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\xinput1_4.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}\steamtools"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}\steamtools"; Flags: ignoreversion

[UninstallDelete]
; 数据目录整体删掉 (含 host.log / lua / pattern 缓存 / update staging).
Type: filesandordirs; Name: "{app}\steamtools"
; .old 是更新 swap 的备份, 卸载时一并清掉.
Type: files; Name: "{app}\stbase.dll.old"

[Code]
function FindWindowW(lpClassName, lpWindowName: string): HWND;
  external 'FindWindowW@user32.dll stdcall';

var
  SteamFound: string;

const
  SteamPathKey = 'Software\Valve\Steam';
  SteamPathValue = 'SteamPath';
  SteamInstallValue = 'InstallPath';

// 去掉首尾空白与包裹引号; Steam 注册表偶发带引号或尾随空格.
function TrimPath(const S: string): string;
var
  L, R: Integer;
begin
  L := 1;
  R := Length(S);
  while (L <= R) and (S[L] <= ' ') do
    L := L + 1;
  while (R >= L) and (S[R] <= ' ') do
    R := R - 1;
  if (L <= R) and (S[L] = '"') and (S[R] = '"') and (R > L) then
  begin
    L := L + 1;
    R := R - 1;
  end;
  Result := Copy(S, L, R - L + 1);
  // 注册表常见 e:/program files/steam, 统一成反斜杠便于后续拼接.
  StringChangeEx(Result, '/', '\', True);
  // 去掉尾部反斜杠, 避免 steam.exe 拼成 ...\\steam.exe.
  while (Length(Result) > 3) and (Result[Length(Result)] = '\') do
    SetLength(Result, Length(Result) - 1);
end;

// 目录里必须有 steam.exe, 才算真正的 Steam 根目录 (排除卸载残留注册表).
function IsSteamRoot(const Dir: string): Boolean;
begin
  Result := (Dir <> '') and DirExists(Dir) and FileExists(AddBackslash(Dir) + 'steam.exe');
end;

// 从单个根键读 SteamPath / InstallPath.
function ReadSteamPathFromRoot(RootKey: Integer): string;
var
  Value: string;
begin
  Result := '';
  if RegQueryStringValue(RootKey, SteamPathKey, SteamPathValue, Value) then
    Result := TrimPath(Value);
  if (Result = '') and RegQueryStringValue(RootKey, SteamPathKey, SteamInstallValue, Value) then
    Result := TrimPath(Value);
end;

// 多源探测: HKCU → HKLM(32) → HKLM64 → 常见默认路径 (仅当该处确有 steam.exe).
// 未装 Steam / 注册表残留 / 路径损坏时返回空串, 由 InitializeSetup 明确拒绝.
function ReadSteamPath(): string;
var
  Candidate: string;
begin
  Result := '';

  Candidate := ReadSteamPathFromRoot(HKCU);
  if IsSteamRoot(Candidate) then
  begin
    Result := Candidate;
    Exit;
  end;

  Candidate := ReadSteamPathFromRoot(HKLM);
  if IsSteamRoot(Candidate) then
  begin
    Result := Candidate;
    Exit;
  end;

  // 64 位系统上部分安装把路径写在非重定向视图.
  if IsWin64 then
  begin
    Candidate := ReadSteamPathFromRoot(HKLM64);
    if IsSteamRoot(Candidate) then
    begin
      Result := Candidate;
      Exit;
    end;
  end;

  // 注册表缺失但 Steam 装在常见位置时兜底; 仍要求 steam.exe 存在.
  // (Inno Pascal 不支持函数内 typed const 数组, 逐个写.)
  Candidate := 'C:\Program Files (x86)\Steam';
  if IsSteamRoot(Candidate) then
  begin
    Result := Candidate;
    Exit;
  end;
  Candidate := 'C:\Program Files\Steam';
  if IsSteamRoot(Candidate) then
  begin
    Result := Candidate;
    Exit;
  end;
  Candidate := 'D:\Steam';
  if IsSteamRoot(Candidate) then
  begin
    Result := Candidate;
    Exit;
  end;
end;

// 只返回已验证的 Steam 根目录; 找不到时给空串, 绝不臆造路径.
function GetSteamDir(Param: string): string;
begin
  Result := SteamFound;
end;

// Steam 主窗口标题就是 "Steam"; 托盘常驻时也有窗口. 检测到就拦安装/卸载.
function IsSteamRunning(): Boolean;
begin
  Result := FindWindowW('', 'Steam') <> 0;
end;

function InitializeSetup(): Boolean;
var
  DiagPath: string;
  RawHkcu, RawHklm: string;
begin
  Result := False;

  // 诊断: 原始注册表值 vs 最终采用路径, 方便用户反馈 "没装 Steam 却怎样" 类问题.
  RawHkcu := ReadSteamPathFromRoot(HKCU);
  RawHklm := ReadSteamPathFromRoot(HKLM);
  SteamFound := ReadSteamPath();
  DiagPath := GetTempDir() + 'steamtools-iss-diag.txt';
  SaveStringToFile(
    DiagPath,
    'RawHKCU=[' + RawHkcu + '] RawHKLM=[' + RawHklm +
    '] SteamFound=[' + SteamFound + '] GetSteamDir=[' + GetSteamDir('') + ']',
    False);

  if SteamFound = '' then
  begin
    // 分三种失败形态, 文案直接可操作.
    if (RawHkcu <> '') or (RawHklm <> '') then
    begin
      if not DirExists(RawHkcu) and not DirExists(RawHklm) then
        MsgBox(
          '注册表里的 Steam 路径已失效 (可能已卸载):' + #13#10 +
          '  HKCU: ' + RawHkcu + #13#10 +
          '  HKLM: ' + RawHklm + #13#10#13#10 +
          '请重新安装 Steam 后再运行本安装器。',
          mbError, MB_OK)
      else
        MsgBox(
          '找到疑似 Steam 目录, 但其中没有 steam.exe:' + #13#10 +
          '  HKCU: ' + RawHkcu + #13#10 +
          '  HKLM: ' + RawHklm + #13#10#13#10 +
          '请修复 / 重装 Steam 后再运行本安装器。SteamTools 必须安装到 Steam 根目录。',
          mbError, MB_OK);
    end
    else
      MsgBox(
        '未检测到 Steam。' + #13#10#13#10 +
        'SteamTools 需要安装到 Steam 根目录 (与 steam.exe 同级), ' +
        '请先安装并至少运行一次 Steam, 再运行本安装器。',
        mbError, MB_OK);
    Exit;
  end;

  // 安装前确认 Steam 已退出, 否则 DLL 被锁, 复制会失败.
  if IsSteamRunning() then
  begin
    MsgBox('检测到 Steam 正在运行。请先完全退出 Steam (含托盘图标), 再继续。', mbError, MB_OK);
    Exit;
  end;
  Result := True;
end;

function InitializeUninstall(): Boolean;
begin
  Result := not IsSteamRunning();
  if not Result then
    MsgBox('检测到 Steam 正在运行。请先完全退出 Steam (含托盘图标), 再卸载。', mbError, MB_OK);
end;
