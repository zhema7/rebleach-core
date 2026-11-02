#Requires -Version 5.1
<#
.SYNOPSIS
  安装 Amyuni usbmmIdd 虚拟显示器驱动（IDD），并插入一台 1920x1080 虚拟监视器。
.DESCRIPTION
  流程与 RustDesk 官方 win_device.rs 一致：
    1. 设备名 = INF 里的 ClassName（Display），不是硬件 ID
    2. x64 的 SP_DEVINFO_DATA 为 32 字节（含 Reserved）
    3. 注册/绑定统一使用 SetupDiCreateDeviceInfoList 返回的句柄
  驱动包若未入库，先用 pnputil 入库，再从 DriverStore 路径绑定。
.PARAMETER DriverDir
  驱动包目录，需含 usbmmidd.inf / usbmmIdd.cat / x64\usbmmIdd.dll
.PARAMETER LogFile
  日志路径，默认 C:\ProgramData\Rebleach\virtual-display.log
#>
[CmdletBinding()]
param(
    [string]$DriverDir = $PSScriptRoot,
    [string]$LogFile = 'C:\ProgramData\Rebleach\virtual-display.log',
    [switch]$NoPlug
)

$ErrorActionPreference = 'Stop'
$logDir = Split-Path -Parent $LogFile
if (-not (Test-Path $logDir)) { New-Item -ItemType Directory -Path $logDir -Force | Out-Null }
function Log($m) { $line = "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $m"; Add-Content -Path $LogFile -Value $line; Write-Host $line }

Log "=== 安装虚拟显示器 usbmmIdd ==="
Log "进程位数: x$(if ([Environment]::Is64BitProcess) {64} else {86})，cbSize=$(if ([IntPtr]::Size -eq 8) {8} else {6})，脚本=$PSCommandPath"

$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { throw '需要管理员权限运行' }

$inf = Join-Path $DriverDir 'usbmmidd.inf'
if (-not (Test-Path $inf)) { throw "找不到驱动 INF: $inf" }

# ---------- 1. 检查是否已装 ----------
$existing = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object { $_.InstanceId -match '^ROOT\\DISPLAY\\' }
if ($existing) {
    Log "已存在 ROOT\DISPLAY 设备: $($existing.InstanceId) ($($existing.FriendlyName))"
}

# ---------- 2. 驱动入库 ----------
if (-not $existing) {
    Log "pnputil 入库驱动..."
    $out = & pnputil.exe /add-driver $inf /install 2>&1
    Log ($out -join ' ')
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 259) {
        throw "pnputil 失败，退出码 $LASTEXITCODE"
    }
    Start-Sleep -Seconds 2

    # 从 DriverStore 找到已入库的 INF（路径带哈希，不能写死）
    $staged = Get-ChildItem 'C:\Windows\System32\DriverStore\FileRepository' -Directory -Filter 'usbmmidd.inf_amd64_*' -ErrorAction SilentlyContinue |
              Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $staged) { throw 'DriverStore 中未找到 usbmmidd 驱动包' }
    $stagedInf = Join-Path $staged.FullName 'usbmmidd.inf'
    Log "使用 DriverStore INF: $stagedInf"

    # ---------- 3. 创建设备节点并绑定驱动 ----------
    if (-not ('DevNode6' -as [type])) {
        Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.ComponentModel;
using System.Runtime.InteropServices;

public class DevNode6 {
    // x64: cbSize(4)+pad(4)+Guid(16)+DevInst(4)+pad(4)+Reserved(8) = 32
    [StructLayout(LayoutKind.Sequential, Pack = 8)]
    public struct SP_DEVINFO_DATA {
        public uint cbSize;
        public Guid ClassGuid;
        public uint DevInst;
        public uint ReservedLow;
        public uint ReservedHigh;
    }

    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool SetupDiGetINFClass(string infName, ref Guid classGuid, StringBuilder className, uint classNameSize, out uint requiredSize);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid classGuid, IntPtr hwnd);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr SetupDiCreateDeviceInfo(IntPtr deviceInfoSet, string deviceName, ref Guid classGuid, string desc, IntPtr hwnd, uint flags, ref SP_DEVINFO_DATA deviceInfoData);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool SetupDiSetDeviceRegistryProperty(IntPtr devInfo, ref SP_DEVINFO_DATA deviceInfoData, uint property, byte[] buffer, uint bufferSize);
    [DllImport("setupapi.dll", SetLastError=true)]
    public static extern bool SetupDiCallClassInstaller(uint installFunction, IntPtr devInfo, ref SP_DEVINFO_DATA deviceInfoData);
    [DllImport("setupapi.dll", SetLastError=true)]
    public static extern bool SetupDiDestroyDeviceInfoList(IntPtr devInfo);
    [DllImport("newdev.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool UpdateDriverForPlugAndPlayDevicesW(IntPtr hwndParent, string hardwareId, string fullInfPath, uint installFlags, out bool rebootRequired);

    public const uint DICD_GENERATE_ID = 0x00000001;
    public const uint SPDRP_HARDWAREID = 0x00000001;
    public const uint DIF_REGISTERDEVICE = 0x00000019;
    public const uint INSTALLFLAG_FORCE = 0x00000001;

    public static string Msg(int e) { try { return new Win32Exception(e).Message; } catch { return "?"; } }

    public static string Run(string infPath, string hwid) {
        Guid g = Guid.Empty;
        StringBuilder cls = new StringBuilder(64);
        uint req;
        if (!SetupDiGetINFClass(infPath, ref g, cls, 64, out req)) {
            int e = Marshal.GetLastWin32Error();
            return "FAIL GetINFClass err=" + e + " (" + Msg(e) + ")";
        }
        string stage = "infclass='" + cls.ToString() + "' guid=" + g + " | ";

        IntPtr h = SetupDiCreateDeviceInfoList(ref g, IntPtr.Zero);
        if (h == IntPtr.Zero) { int e = Marshal.GetLastWin32Error(); return stage + "FAIL CreateList err=" + e + " (" + Msg(e) + ")"; }
        try {
            SP_DEVINFO_DATA did = new SP_DEVINFO_DATA();
            did.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVINFO_DATA));

            IntPtr h2 = SetupDiCreateDeviceInfo(h, cls.ToString(), ref g, null, IntPtr.Zero, DICD_GENERATE_ID, ref did);
            if (h2 == (IntPtr)(-1)) { int e = Marshal.GetLastWin32Error(); return stage + "FAIL CreateDeviceInfo(class) err=" + e + " (" + Msg(e) + ")"; }
            stage += "devnode-created; ";

            byte[] hw = Encoding.Unicode.GetBytes(hwid + "\0");
            if (!SetupDiSetDeviceRegistryProperty(h, ref did, SPDRP_HARDWAREID, hw, (uint)hw.Length)) {
                int e = Marshal.GetLastWin32Error(); return stage + "FAIL SetHWID err=" + e + " (" + Msg(e) + ")";
            }
            stage += "hwid-set; ";

            if (!SetupDiCallClassInstaller(DIF_REGISTERDEVICE, h, ref did)) {
                int e = Marshal.GetLastWin32Error(); return stage + "FAIL Register err=" + e + " (" + Msg(e) + ")";
            }
            stage += "registered; ";

            bool reboot;
            if (!UpdateDriverForPlugAndPlayDevicesW(IntPtr.Zero, hwid, infPath, INSTALLFLAG_FORCE, out reboot)) {
                int e = Marshal.GetLastWin32Error(); return stage + "FAIL UpdateDriver err=" + e + " (" + Msg(e) + ")";
            }
            return stage + "UpdateDriver OK reboot=" + reboot;
        } finally {
            SetupDiDestroyDeviceInfoList(h);
        }
    }
}
"@
    }

    $r = [DevNode6]::Run($stagedInf, 'usbmmIdd')
    Log "设备节点: $r"
    if ($r -notmatch 'UpdateDriver OK') { throw "创建设备节点失败: $r" }
}

# ---------- 4. 插入虚拟监视器 ----------
if (-not ('PlugMon' -as [type])) {
    Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.ComponentModel;
using System.Runtime.InteropServices;

public class PlugMon {
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr SetupDiGetClassDevs(ref Guid classGuid, string enumerator, IntPtr hwnd, uint flags);
    [DllImport("setupapi.dll", SetLastError=true)]
    public static extern bool SetupDiEnumDeviceInterfaces(IntPtr devInfo, IntPtr devInfoData, ref Guid interfaceClassGuid, uint memberIndex, ref SP_DEVICE_INTERFACE_DATA ifData);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern bool SetupDiGetDeviceInterfaceDetail(IntPtr devInfo, ref SP_DEVICE_INTERFACE_DATA ifData, IntPtr detailData, uint detailSize, out uint requiredSize, IntPtr devInfoData);
    [DllImport("setupapi.dll", SetLastError=true)]
    public static extern bool SetupDiDestroyDeviceInfoList(IntPtr devInfo);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr CreateFile(string lpFileName, uint desiredAccess, uint shareMode, IntPtr sec, uint creationDisposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError=true)]
    public static extern bool DeviceIoControl(IntPtr hDevice, uint ioControlCode, byte[] inBuffer, uint inBufferSize, byte[] outBuffer, uint outBufferSize, out uint bytesReturned, IntPtr overlapped);
    [DllImport("kernel32.dll")]
    public static extern bool CloseHandle(IntPtr h);

    [StructLayout(LayoutKind.Sequential)]
    public struct SP_DEVICE_INTERFACE_DATA {
        public uint cbSize;
        public Guid InterfaceClassGuid;
        public uint Flags;
        public IntPtr Reserved;
    }

    public const uint DIGCF_PRESENT = 0x00000002;
    public const uint DIGCF_DEVICEINTERFACE = 0x00000010;
    public const uint GENERIC_READ = 0x80000000;
    public const uint GENERIC_WRITE = 0x40000000;
    public const uint OPEN_EXISTING = 3;
    public const uint IOCTL_PLUG = 2307084;

    public static string Msg(int e) { try { return new Win32Exception(e).Message; } catch { return "?"; } }

    // cmd 0x10 = plug in, 0x00 = plug out
    public static string Run(uint cmd) {
        Guid g = new Guid("{B5FFD75F-DA40-4353-8FF8-B6DAF6F1D8CA}");
        IntPtr h = SetupDiGetClassDevs(ref g, null, IntPtr.Zero, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if (h == (IntPtr)(-1)) { int e = Marshal.GetLastWin32Error(); return "FAIL GetClassDevs err=" + e + " (" + Msg(e) + ")"; }
        try {
            SP_DEVICE_INTERFACE_DATA ifd = new SP_DEVICE_INTERFACE_DATA();
            ifd.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVICE_INTERFACE_DATA));
            if (!SetupDiEnumDeviceInterfaces(h, IntPtr.Zero, ref g, 0, ref ifd)) {
                int e = Marshal.GetLastWin32Error();
                return (e == 259) ? "NO_INTERFACE" : "FAIL EnumInterfaces err=" + e + " (" + Msg(e) + ")";
            }
            uint req;
            SetupDiGetDeviceInterfaceDetail(h, ref ifd, IntPtr.Zero, 0, out req, IntPtr.Zero);
            IntPtr buf = Marshal.AllocHGlobal((int)req);
            try {
                // cbSize 必须按进程位数取值：x64=8, x86=6。写死 8 会让 32 位 PowerShell
                // 调用 GetDetail 时返回 ERROR_INVALID_USER_BUFFER (1784)
                Marshal.WriteInt32(buf, 0, (IntPtr.Size == 8) ? 8 : 6);
                if (!SetupDiGetDeviceInterfaceDetail(h, ref ifd, buf, req, out req, IntPtr.Zero)) {
                    int e = Marshal.GetLastWin32Error(); return "FAIL GetDetail err=" + e + " (" + Msg(e) + ")";
                }
                string path = Marshal.PtrToStringUni(new IntPtr(buf.ToInt64() + 4));
                if (string.IsNullOrEmpty(path)) return "FAIL empty device path";
                IntPtr hd = CreateFile(path, GENERIC_READ | GENERIC_WRITE, 0, IntPtr.Zero, OPEN_EXISTING, 0, IntPtr.Zero);
                if (hd == (IntPtr)(-1) || hd == IntPtr.Zero) { int e = Marshal.GetLastWin32Error(); return "FAIL CreateFile err=" + e + " (" + Msg(e) + ")"; }
                try {
                    byte[] inb = new byte[4] { (byte)cmd, 0, 0, 0 };
                    uint ret;
                    if (!DeviceIoControl(hd, IOCTL_PLUG, inb, 4, null, 0, out ret, IntPtr.Zero)) {
                        int e = Marshal.GetLastWin32Error(); return "FAIL IoControl err=" + e + " (" + Msg(e) + ")";
                    }
                    return "IOCTL OK cmd=0x" + cmd.ToString("X");
                } finally { CloseHandle(hd); }
            } finally { Marshal.FreeHGlobal(buf); }
        } finally { SetupDiDestroyDeviceInfoList(h); }
    }
}
"@
}

if ($NoPlug) {
    Log '按参数跳过插屏 (-NoPlug)：重复运行本脚本时用它，避免多插出虚拟屏'
} else {
    $r = [PlugMon]::Run(0x10)
    Log "插入监视器: $r"
}

Start-Sleep -Seconds 2
$mons = Get-PnpDevice -Class Monitor -ErrorAction SilentlyContinue
Log "当前监视器数量: $($mons.Count)"
$mons | ForEach-Object { Log "  $($_.Status) | $($_.FriendlyName) | $($_.InstanceId)" }

Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.Screen]::AllScreens | ForEach-Object { Log "  屏幕 $($_.DeviceName) $($_.Bounds.Width)x$($_.Bounds.Height) Primary=$($_.Primary)" }
Log "=== 完成 ==="
