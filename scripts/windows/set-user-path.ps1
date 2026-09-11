# Append or remove one directory from the per-user PATH (HKCU\Environment\Path).
#
# Called by hydra-installer.nsi instead of doing the registry work there,
# because NSIS runtime strings cap at NSIS_MAX_STRLEN (1024 characters in
# official builds). A longer PATH comes back EMPTY from ReadRegStr, which made
# the installer's old read-append-write block replace the whole value with
# $INSTDIR, and made the uninstaller write an empty PATH. .NET registry APIs
# have no length limit; reading unexpanded keeps %VAR%-style entries intact.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File set-user-path.ps1 -Action Append -Dir "C:\some\dir"
#   powershell -NoProfile -ExecutionPolicy Bypass -File set-user-path.ps1 -Action Remove -Dir "C:\some\dir"
#
# The change is minimal by design: anything already in the value keeps its
# position and spelling. Append is a no-op when the directory is present in
# any spelling (case, quoting, trailing backslash); Remove is a no-op when it
# is absent. Running processes are notified by the installer itself (a plain
# SendMessage there) rather than from here, so this script needs no Add-Type.
#
# Exit codes: 0 = the PATH is in the wanted state (written or already was),
# 1 = something failed and nothing was written.

param(
  [Parameter(Mandatory = $true)][ValidateSet('Append', 'Remove')][string]$Action,
  [Parameter(Mandatory = $true)][string]$Dir
)

$ErrorActionPreference = 'Stop'
$key = $null
try {
  if ([string]::IsNullOrWhiteSpace($Dir)) { exit 1 }

  # CreateSubKey, not OpenSubKey: an absent Environment key is created for
  # writing, mirroring what WriteRegExpandStr did.
  $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
  if (-not $key) { exit 1 }

  # Read the raw value unexpanded so %VAR% entries round-trip unchanged, and
  # write back in the value's own kind (Path is REG_EXPAND_SZ by default; an
  # absent value is created as REG_EXPAND_SZ, like WriteRegExpandStr did).
  $opts = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
  $current = [string]$key.GetValue('Path', '', $opts)
  $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
  try { $kind = $key.GetValueKind('Path') } catch { }

  $dirNorm = $Dir.Trim().Trim('"').TrimEnd('\').ToLowerInvariant()
  $entries = @()
  if ($current -ne '') {
    $entries = @($current -split ';' | Where-Object { $_.Trim() -ne '' })
  }

  $present = $entries | Where-Object {
    $_.Trim().Trim('"').TrimEnd('\').ToLowerInvariant() -eq $dirNorm
  }

  if ($Action -eq 'Append') {
    # Already present in any spelling: keep the user's own ordering and
    # formatting untouched rather than rewriting the value.
    if ($present) { exit 0 }
    $new = if ($current -ne '') {
      if ($current.EndsWith(';')) { "$current$Dir" } else { "$current;$Dir" }
    } else {
      $Dir
    }
  } else {
    # Not present: nothing to remove, leave the value exactly as it is.
    if (-not $present) { exit 0 }
    $kept = @($entries | Where-Object {
      $_.Trim().Trim('"').TrimEnd('\').ToLowerInvariant() -ne $dirNorm
    })
    $new = ($kept -join ';')
  }

  $key.SetValue('Path', $new, $kind)
} catch {
  exit 1
} finally {
  if ($key) { $key.Close() }
}
exit 0
