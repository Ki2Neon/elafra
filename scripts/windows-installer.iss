[Setup]
AppId=app.elafra.Elafra
AppName=Elafra
AppVersion={#PackageVersion}
AppPublisher=Ki2Neon
AppPublisherURL=https://github.com/Ki2Neon/elafra
DefaultDirName={autopf}\Elafra
PrivilegesRequired=lowest
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.19045
UninstallDisplayIcon={app}\elafra.exe
WizardStyle=modern
Compression=lzma2
SolidCompression=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ja"; MessagesFile: "compiler:Languages\Japanese.isl"

[Files]
Source: "{#BundleDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Elafra"; Filename: "{app}\elafra.exe"; WorkingDir: "{app}"
