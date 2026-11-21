# g6-run.mjs verifies provenance before calling this driver. Only the QA bridge
# is installed directly. Neither Tauri executable is launched by this script.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Blocked([string]$reason) { throw "BLOCKED G6 Windows: $reason" }

function LocalItem([string]$path) {
  if ($path -notmatch '^[A-Za-z]:\\' -or $path.Substring(2) -match '[:*?]' -or
      [IO.Path]::GetFullPath($path) -ine $path) { Blocked 'expected a canonical local path' }
  $item = Get-Item -Force -LiteralPath $path
  for ($p = $item; $null -ne $p;) {
    if ($p.Attributes -band [IO.FileAttributes]::ReparsePoint) { Blocked "redirected path: $path" }
    if ($p -is [IO.FileInfo]) { $p = $p.Directory } else { $p = $p.Parent }
  }
  return $item
}

function QaItem([string]$path, [bool]$directory = $false) {
  if (!$path.StartsWith("$qaRoot\", [StringComparison]::OrdinalIgnoreCase)) {
    Blocked 'all staged inputs and output must be inside RUNNER_TEMP\g6-qa'
  }
  $item = LocalItem $path
  if ($item.PSIsContainer -ne $directory) { Blocked "unexpected file type: $path" }
  return $item
}

function WaitFor([string]$what, [int]$seconds, [scriptblock]$read, [switch]$Control) {
  $clock = [Diagnostics.Stopwatch]::StartNew()
  do {
    $value = & $read
    if ($value) { return $value }
    Start-Sleep -Milliseconds 500
  } while ($clock.Elapsed.TotalSeconds -lt $seconds)
  if ($Control) { Blocked "no accessible $what after ${seconds}s" }
  throw "did not observe $what after ${seconds}s"
}

# Search only top-level windows of the exact installed process, including menus
# and secondary update windows. A renderer or another app's matching label is
# never enough on its own.
function Control($process, [string[]]$names, $type, $parent = $null) {
  if ($process.HasExited) { throw 'the installed app exited while reading its UI' }
  $roots = if ($null -ne $parent) { $parent } else {
    $owner = [System.Windows.Automation.PropertyCondition]::new(
      [System.Windows.Automation.AutomationElement]::ProcessIdProperty, [int]$process.Id)
    [System.Windows.Automation.AutomationElement]::RootElement.FindAll('Children', $owner)
  }
  foreach ($root in $roots) {
    foreach ($name in $names) {
      $byName = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::NameProperty, $name)
      try { $nodes = $root.FindAll('Subtree', $byName) }
      catch [System.Windows.Automation.ElementNotAvailableException] { continue }
      foreach ($node in $nodes) {
        try {
          $state = $node.Current
          if (!$state.IsOffscreen -and $state.IsEnabled -and
              (!$type -or $state.ControlType -eq $type)) { return $node }
        } catch [System.Windows.Automation.ElementNotAvailableException] { continue }
      }
    }
  }
}

function Press($control) {
  $pattern = $null
  if (!$control.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
    Blocked "control does not support Invoke: $($control.Current.Name)"
  }
  $pattern.Invoke()
}

function AppIdentity([string]$version) {
  $info = (LocalItem $exe).VersionInfo
  if ($info.ProductName -cne 'SeuOffice' -or $info.ProductVersion -cnotin @($version, "$version.0")) {
    throw "installed PE identity is $($info.ProductName) $($info.ProductVersion), expected SeuOffice $version"
  }
  if ((Get-AuthenticodeSignature -LiteralPath $exe).Status -ne 'NotSigned') {
    throw 'installed PE differs from the expected unsigned Windows QA signing mode'
  }
  $yml = LocalItem (Join-Path $install 'resources\app-update.yml')
  $actual = @([IO.File]::ReadAllLines($yml.FullName) | Where-Object { $_.Trim() } | Sort-Object)
  $expected = @("channel: $($plan.channel)", 'provider: generic', 'updaterCacheDirName: seuoffice-updater', 'url: http://127.0.0.1:8765/' | Sort-Object)
  if (Compare-Object $actual $expected -CaseSensitive) { throw 'installed app is not the fixed loopback QA channel' }
}

function RecentOnDisk {
  if (!(Test-Path -LiteralPath $recentFile -PathType Leaf)) { return $false }
  $json = [IO.File]::ReadAllText((LocalItem $recentFile).FullName)
  return ($json.TrimStart().StartsWith('[') -and ($json | ConvertFrom-Json) -icontains $document)
}

function RecentInApp($process) {
  $row = Control $process @("Select $leaf", "$leaf 선택") ([System.Windows.Automation.ControlType]::CheckBox)
  for ($node = $row; $null -ne $node; $node = [System.Windows.Automation.TreeWalker]::RawViewWalker.GetParent($node)) {
    if ($node.Current.Name -cin @('Recent','최근 사용')) { return $row }
    if ($node.Current.ControlType -eq [System.Windows.Automation.ControlType]::Window) { break }
  }
}

function DialogAction($process, [string]$version, [string[]]$labels) {
  $dialog = Control $process @('Software Update', '소프트웨어 업데이트') ([System.Windows.Automation.ControlType]::Window)
  if ($dialog -and (Control $process @("v$version") $null $dialog)) {
    $errorAction = Control $process @('Retry','다시 시도','Open Download Page','다운로드 페이지 열기') $button $dialog
    if ($errorAction) { throw "app offered $($errorAction.Current.Name) instead of completing the update" }
    return (Control $process $labels $button $dialog)
  }
}

try {
  if ([Environment]::OSVersion.Platform -ne 'Win32NT' -or
      $env:GITHUB_ACTIONS -cne 'true' -or $env:RUNNER_ENVIRONMENT -cne 'github-hosted' -or
      $env:GITHUB_EVENT_NAME -cne 'workflow_dispatch' -or $env:GITHUB_REPOSITORY -cne 'seuput-inc/seuoffice' -or
      $env:RUNNER_OS -cne 'Windows' -or $env:RUNNER_ARCH -cne 'X64' -or
      $env:GITHUB_RUN_ID -notmatch '^[1-9]\d*$' -or $env:GITHUB_RUN_ATTEMPT -notmatch '^[1-9]\d*$') {
    Blocked 'requires a disposable GitHub-hosted Windows x64 dispatch'
  }
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $homeDir = [Environment]::GetFolderPath('UserProfile')
  $roaming = [Environment]::GetFolderPath('ApplicationData')
  $local = [Environment]::GetFolderPath('LocalApplicationData')
  if ($identity.Name -ine "$env:COMPUTERNAME\runneradmin" -or $homeDir -ine 'C:\Users\runneradmin' -or
      $env:USERPROFILE -ine $homeDir -or $roaming -ine "$homeDir\AppData\Roaming" -or
      $env:APPDATA -ine $roaming -or $local -ine "$homeDir\AppData\Local" -or $env:LOCALAPPDATA -ine $local -or
      $env:SEUOFFICE_USER_DATA -or $env:ELECTRON_RUN_AS_NODE -or $env:NODE_OPTIONS) {
    Blocked 'default runner account, profile and launch environment required'
  }
  $null = LocalItem $roaming
  $null = LocalItem $local
  $qaRoot = (LocalItem (Join-Path $env:RUNNER_TEMP 'g6-qa')).FullName
  if ($qaRoot.StartsWith("$homeDir\", [StringComparison]::OrdinalIgnoreCase)) { Blocked 'QA output is inside user profile' }
  $plan = [IO.File]::ReadAllText((QaItem $env:G6_PLAN).FullName) | ConvertFrom-Json
  $out = (QaItem $env:G6_OUT $true).FullName
  $controlFile = (QaItem $env:G6_FEED_CONTROL).FullName
  if ($plan.channel -cnotin @('latest','stable') -or [IO.File]::ReadAllText($controlFile).Trim() -cne 'first') {
    Blocked 'expected one selected channel and a fresh first-stage feed'
  }
  $stable = if ($plan.channel -ceq 'stable') { '-stable' } else { '' }
  foreach ($stage in 'bridge','first','next') {
    $v = $plan.$stage.version
    if ($v -notmatch '^\d+\.\d+\.\d+$') { Blocked 'invalid version' }
    $suffix = if ($stage -eq 'bridge') { "$stable-migration-seam" } else { "-qa-loopback$stable" }
    if ((QaItem $plan.$stage.installer).Name -cne "SeuOffice-Setup-$v$suffix.exe") {
      Blocked "not the expected $stage QA-only installer"
    }
  }
  if ([version]$plan.bridge.version -ge [version]$plan.first.version -or
      [version]$plan.first.version -ge [version]$plan.next.version) { Blocked 'versions do not strictly increase' }
  $profileDir = Join-Path $roaming 'seuoffice'
  $recentFile = Join-Path $profileDir 'recent.json'
  $cache = Join-Path $local 'seuoffice-updater'
  $install = Join-Path $local 'Programs\SeuOffice'
  $exe = Join-Path $install 'SeuOffice.exe'
  foreach ($dir in @($profileDir,$cache,$install,(Join-Path $local 'SeuOffice'),(Join-Path $env:ProgramFiles 'SeuOffice'))) {
    if (Test-Path -LiteralPath $dir) { Blocked "pre-existing app state at $dir" }
  }
  foreach ($hive in 'HKCU:\Software','HKLM:\Software','HKLM:\Software\WOW6432Node') {
    $key = "$hive\Microsoft\Windows\CurrentVersion\Uninstall"
    if (Test-Path -LiteralPath $key) {
      foreach ($entry in (Get-ChildItem -LiteralPath $key)) {
        if ($entry.PSChildName -ieq 'SeuOffice' -or $entry.GetValue('DisplayName','') -like 'SeuOffice*') {
          Blocked 'existing registered installation'
        }
      }
    }
  }
  if (Get-Process -Name SeuOffice -ErrorAction SilentlyContinue) { Blocked 'app already running' }

  $leaf = 'g6-' + [guid]::NewGuid().ToString('N') + '.docx'
  $document = Join-Path $out $leaf
  & node (Join-Path $PSScriptRoot 'probe.mjs') make-docx $document
  if ($LASTEXITCODE -ne 0) { throw 'could not create synthetic document' }
  $digest = (Get-FileHash -LiteralPath $document -Algorithm SHA256).Hash.ToLowerInvariant()
  $setup = Start-Process -FilePath $plan.bridge.installer -ArgumentList '/S','/currentuser' -PassThru
  if (!$setup.WaitForExit(600000) -or $setup.ExitCode -ne 0) { throw 'direct bridge install failed' }
  AppIdentity $plan.bridge.version
  if (Get-Process -Name SeuOffice -ErrorAction SilentlyContinue) { Blocked 'unexpected auto-start by bridge setup' }
  $app = Start-Process -FilePath $exe -ArgumentList "`"$document`"" -PassThru
  $null = WaitFor 'the bridge to record the exact open path in its default profile' 120 { RecentOnDisk }
  try {
    Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes
    $button = [System.Windows.Automation.ControlType]::Button
  } catch { Blocked "Windows UI Automation unavailable: $($_.Exception.Message)" }
  $close = WaitFor 'bridge document tab Close button' 90 { Control $app @('Close tab','탭 닫기') $button } -Control
  Press $close
  $homeControl = WaitFor 'bridge onboarding Skip or Recent document row' 90 {
    $skip = Control $app @('Skip','건너뛰기') $button
    if ($skip) { return $skip }
    RecentInApp $app
  } -Control
  if ($homeControl.Current.ControlType -eq $button) {
    Press $homeControl
    $null = WaitFor 'bridge Recent document row' 90 { RecentInApp $app } -Control
  }

  foreach ($stage in 'first','next') {
    $target = $plan.$stage
    if ($stage -eq 'next') {
      $staged = "$controlFile.next"
      if (Test-Path -LiteralPath $staged) { Blocked 'stage switch already present' }
      [IO.File]::WriteAllText($staged, 'next')
      [IO.File]::Move($staged, $controlFile, $true)
      if (!(DialogAction $app $target.version @('Update Now','지금 업데이트'))) {
        $menu = WaitFor 'app menu button' 60 { Control $app @('Menu','메뉴') $button } -Control
        Press $menu
        $check = WaitFor 'Check for Updates menu item' 30 {
          Control $app @('Check for Updates…','업데이트 확인…') ([System.Windows.Automation.ControlType]::MenuItem)
        } -Control
        Press $check
      }
    }
    $download = WaitFor "$stage update dialog offering v$($target.version)" 120 {
      DialogAction $app $target.version @('Update Now','지금 업데이트')
    } -Control
    Press $download
    $restart = WaitFor "$stage Restart & Install control" 600 {
      DialogAction $app $target.version @('Restart & Install','다시 시작 및 설치')
    } -Control
    $pending = @(Get-ChildItem -LiteralPath (Join-Path $cache 'pending') -File -Filter '*.exe')
    if ($pending.Count -ne 1 -or
        (Get-FileHash -LiteralPath (LocalItem $pending[0].FullName).FullName).Hash -cne (Get-FileHash -LiteralPath $target.installer).Hash) {
      throw "$stage downloaded installer bytes differ from the verified QA artifact"
    }
    $before = [datetime]::UtcNow
    Press $restart
    $installer = WaitFor "$stage updater to launch the verified pending installer" 120 {
      foreach ($candidate in (Get-Process -Name $pending[0].BaseName -ErrorAction SilentlyContinue)) {
        if ($candidate.Path -ieq $pending[0].FullName -and $candidate.StartTime.ToUniversalTime() -ge $before) {
          $null = $candidate.Handle
          return $candidate
        }
      }
    }
    if (!$app.WaitForExit(120000)) { throw "$stage update did not exit the old process" }
    if (!$installer.WaitForExit(600000) -or $installer.ExitCode -ne 0) { throw "$stage updater installer failed" }
    $app = WaitFor "$stage installer to start the new --updated app" 600 {
      foreach ($proc in (Get-CimInstance Win32_Process -Filter "Name = 'SeuOffice.exe'")) {
        if ($proc.ExecutablePath -ine $exe -or $proc.CreationDate.ToUniversalTime() -lt $before -or
            $proc.CommandLine -notmatch '(?:^|\s)"?--updated"?(?:\s|$)' -or $proc.CommandLine -match '(?:^|\s)--type=') { continue }
        $owner = Invoke-CimMethod -InputObject $proc -MethodName GetOwnerSid
        if ($owner.ReturnValue -ne 0 -or $owner.Sid -ne $identity.User.Value) { throw 'relaunched under another account' }
        return (Get-Process -Id $proc.ProcessId -ErrorAction SilentlyContinue)
      }
    }
    $null = WaitFor "$stage installer commit and app start mark" 150 {
      $mark = Join-Path $profileDir 'shell-started.txt'
      (Test-Path -LiteralPath (Join-Path $install '.seuoffice-install-complete') -PathType Leaf) -and
        (Test-Path -LiteralPath $mark -PathType Leaf) -and [IO.File]::ReadAllText((LocalItem $mark).FullName).Trim() -ceq $target.version
    }
    AppIdentity $target.version
    $null = WaitFor "$stage Recent document row in relaunched app" 90 { RecentInApp $app } -Control
    if ($app.HasExited -or !(RecentOnDisk) -or (Get-FileHash -LiteralPath $document).Hash -ine $digest) {
      throw "$stage did not preserve the document and default profile"
    }
    $arrival = [ordered]@{stage=$stage; version=$target.version; profile=$profileDir; document=$digest; recent=$true}
    Write-Output ('G6_EVENT ' + ($arrival | ConvertTo-Json -Compress))
  }
} catch {
  $message = $_.Exception.Message
  if ($message.StartsWith('BLOCKED G6')) {
    Write-Output ('G6_BLOCKED ' + ($message -replace '^BLOCKED G6 Windows: ', ''))
    [Console]::Error.WriteLine($message)
    exit 3
  }
  [Console]::Error.WriteLine("FAILED G6 Windows: $message")
  exit 1
}
