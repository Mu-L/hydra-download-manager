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
# The directory is matched case-insensitively and without trailing
# backslashes, in either direction: appending a directory that is already
# present in another spelling is a no-op rather than a duplicate.
#
# Exit codes: 0 = the PATH is in the wanted state (written or already was),
# 1 = something failed and nothing was written.

param(
  [Parameter(Mandatory = $true)][ValidateSet('Append', 'Remove')][string]$Action,
  [Parameter(Mandatory = $true)][string]$Dir
)

$ErrorActionPreference = 'Stop'
try {
  if ([string]::IsNullOrWhiteSpace($Dir)) { exit 1 }

  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
  if (-not $key) { exit 1 }

  # Read the raw value unexpanded so %VAR% entries round-trip unchanged, and
  # write back in the value's own kind (Path is REG_EXPAND_SZ by default; an
  # absent value is created as REG_EXPAND_SZ, like WriteRegExpandStr did).
  $opts = [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames
  $current = [string]$key.GetValue('Path', '', $opts)
  $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
  try { $kind = $key.GetValueKind('Path') } catch { }

  $dirNorm = $Dir.TrimEnd('\').ToLowerInvariant()
  $entries = @()
  if ($current -ne '') {
    $entries = @($current -split ';' | Where-Object { $_ -ne '' })
  }
  $kept = @($entries | Where-Object { $_.TrimEnd('\').ToLowerInvariant() -ne $dirNorm })
  if ($Action -eq 'Append') { $kept += $Dir }

  $new = ($kept -join ';')
  if ($new -eq $current) { exit 0 }

  $key.SetValue('Path', $new, $kind)

  # Registry writes do not notify running processes. Tell Explorer and friends
  # so newly started shells pick the change up without a logoff; failures
  # here must not turn a successful write into a reported error.
  try {
    Add-Type -Namespace Win32 -Name HydraPathBroadcast -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", SetLastError = true)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr hWnd, uint Msg, System.UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out System.UIntPtr lpdwResult);
'@
    $smResult = [UIntPtr]::Zero
    [Win32.HydraPathBroadcast]::SendMessageTimeout([IntPtr]0xFFFF, 0x1A, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$smResult) | Out-Null
  } catch { }
} catch {
  exit 1
}
exit 0
