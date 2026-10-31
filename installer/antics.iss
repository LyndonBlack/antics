; Inno Setup script for the antics Windows installer.
;
; Built by the release workflow:  ISCC /DAppVersion=0.1.3 /DBinaryPath=<path to antics.exe> installer\antics.iss
; Per-user install (no administrator rights). The installer contains antics.exe and its docs only:
; NO original game data. It asks where the user's own copy of the game is and runs
;   antics.exe --install "<that folder>"
; which copies nine files into %APPDATA%\antics after checking each one's size and SHA-256.
;
; Unattended use:  setup.exe /VERYSILENT /DIR="C:\path" /GAMEDIR="D:\svs3"   (GAMEDIR optional)

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinaryPath
  #define BinaryPath "..\target\x86_64-pc-windows-msvc\release\antics.exe"
#endif

[Setup]
AppId={{B6E1C2A4-5D1F-4B7E-9A3C-2F6D8E0A7C11}
AppName=antics
AppVersion={#AppVersion}
AppVerName=antics {#AppVersion}
AppPublisher=Mostlysane
AppPublisherURL=https://github.com/LyndonBlack/antics
AppSupportURL=https://github.com/LyndonBlack/antics
DefaultDirName={autopf}\antics
DefaultGroupName=antics
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
InfoBeforeFile=before-install.txt
LicenseFile=..\LICENSE
OutputDir=..\out
OutputBaseFilename=antics-{#AppVersion}-windows-x86_64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\antics.exe
UninstallDisplayName=antics
DisableWelcomePage=no

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "{#BinaryPath}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\docs\*"; DestDir: "{app}\docs"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\antics"; Filename: "{app}\antics.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\antics"; Filename: "{app}\antics.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\antics.exe"; Description: "Launch antics now"; Flags: nowait postinstall skipifsilent; Check: GameReady

[Code]
var
  GamePage: TInputDirWizardPage;
  GameInstalled: Boolean;

function GameFolderOk(Dir: String): Boolean;
begin
  Result := FileExists(AddBackslash(Dir) + 'spy.exe') and FileExists(AddBackslash(Dir) + 'TRAP.PIC');
end;

procedure InitializeWizard;
begin
  GamePage := CreateInputDirPage(wpSelectDir,
    'Your copy of the original game',
    'Where are the original game files?',
    'antics does not include the game. Select the folder that holds your own copy of ' +
    'Spy vs. Spy III: Arctic Antics (the folder with SPY.EXE and TRAP.PIC in it). ' +
    'Leave the box empty to set this up later.',
    False, '');
  GamePage.Add('Folder with your copy of the game:');
  GamePage.Values[0] := ExpandConstant('{param:GAMEDIR|}');
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  D: String;
begin
  Result := True;
  if CurPageID = GamePage.ID then
  begin
    D := Trim(GamePage.Values[0]);
    if (D <> '') and (not GameFolderOk(D)) then
    begin
      MsgBox('That folder does not contain SPY.EXE and TRAP.PIC.' + #13#10#13#10 +
             'Pick the folder where your copy of Spy vs. Spy III: Arctic Antics is unpacked, ' +
             'or leave the box empty to set this up later.', mbError, MB_OK);
      Result := False;
    end;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  D: String;
  Code: Integer;
begin
  if CurStep = ssPostInstall then
  begin
    D := Trim(GamePage.Values[0]);
    if D = '' then
      Exit;
    D := RemoveBackslashUnlessRoot(D);
    if Copy(D, Length(D), 1) = '\' then
      D := D + '.';
    if Exec(ExpandConstant('{app}\antics.exe'), '--install "' + D + '"', '', SW_HIDE,
            ewWaitUntilTerminated, Code) and (Code = 0) then
      GameInstalled := True
    else
      MsgBox('antics was installed, but the game files in that folder were not accepted.' + #13#10#13#10 +
             'They must be the supported release of Spy vs. Spy III: Arctic Antics (all nine files: ' +
             'SPY.EXE, SPYTITL.EXE, TITLE.DAT, TRAP.PIC, MAXX.PIC, DEMO.DAT, TEXT1.DAT, TEXT2.DAT, TEXT3.DAT). ' +
             'See YOU-NEED-THE-ORIGINAL.md in the antics folder. You can run the installer again ' +
             'and choose another folder.', mbError, MB_OK);
  end;
end;

{ Offer to launch only when the game files are in place (just installed, or from an earlier install). }
function GameReady: Boolean;
begin
  Result := GameInstalled or FileExists(ExpandConstant('{userappdata}\antics\antics-install.json'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  P: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    P := ExpandConstant('{userappdata}\antics');
    if DirExists(P) and (not UninstallSilent) then
      if MsgBox('Also remove the copy of the game files that antics made in' + #13#10 + P + ' ?' + #13#10#13#10 +
                'Your original game folder is not touched.', mbConfirmation, MB_YESNO) = IDYES then
        DelTree(P, True, True, True);
  end;
end;
