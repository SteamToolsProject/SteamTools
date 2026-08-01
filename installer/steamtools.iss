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
function FindWindowW(lpClassName, lpWindowName: string): LongPtr;
  external 'FindWindowW@user32.dll stdcall';

var
  SteamDirPage: TInputDirWizardPage;
  SteamFound: string;

const
  SteamPathKey = 'Software\Valve\Steam';
  SteamPathValue = 'SteamPath';

function GetSteamDir(Param: string): string;
begin
  if SteamDirPage <> nil then
    Result := SteamDirPage.Values[0]
  else if SteamFound <> '' then
    Result := SteamFound
  else
    Result := 'C:\Program Files (x86)\Steam';
end;

function ReadSteamPath(): string;
var
  Value: string;
begin
  Result := '';
  if RegQueryStringValue(HKCU, SteamPathKey, SteamPathValue, Value) then
    Result := Value;
  // 部分系统 Steam 把路径写在 HKLM.
  if (Result = '') and RegQueryStringValue(HKLM, SteamPathKey, SteamPathValue, Value) then
    Result := Value;
end;

// Steam 主窗口标题就是 "Steam"; 托盘常驻时也有窗口. 检测到就拦安装/卸载.
function IsSteamRunning(): Boolean;
begin
  Result := FindWindowW('', 'Steam') <> 0;
end;

function InitializeSetup(): Boolean;
begin
  Result := True;
  SteamFound := ReadSteamPath();
end;

procedure InitializeWizard();
begin
  SteamDirPage := CreateInputDirPage(
    wpWelcome, '选择 Steam 安装目录', 'SteamTools 需要把文件放进 Steam 根目录 (steam.exe 所在目录)',
    '如果列表里没有你的 Steam 目录, 请手动选择。点击下一步继续。', False, '');
  SteamDirPage.Add('Steam 根目录:');
  SteamDirPage.Values[0] := GetSteamDir('');
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = SteamDirPage.ID) and not DirExists(SteamDirPage.Values[0]) then
  begin
    MsgBox('目录不存在: ' + SteamDirPage.Values[0], mbError, MB_OK);
    Result := False;
    Exit;
  end;
  // 安装前确认 Steam 已退出, 否则 DLL 被锁, 复制会失败.
  if (CurPageID = SteamDirPage.ID) and IsSteamRunning() then
  begin
    MsgBox('检测到 Steam 正在运行。请先完全退出 Steam (含托盘图标), 再继续。', mbError, MB_OK);
    Result := False;
  end;
end;

function InitializeUninstall(): Boolean;
begin
  Result := not IsSteamRunning();
  if not Result then
    MsgBox('检测到 Steam 正在运行。请先完全退出 Steam (含托盘图标), 再卸载。', mbError, MB_OK);
end;
