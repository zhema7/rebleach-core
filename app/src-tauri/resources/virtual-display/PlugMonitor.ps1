<#
.SYNOPSIS
  只做插/拔虚拟监视器（usbmmidd IOCTL_PLUG），不安装驱动。
.DESCRIPTION
  被 Rebleach Agent 作为 Rust 原生 DeviceIoControl 失败时的兜底路径调用。
  与 Install-VirtualDisplay.ps1 的 PlugMon::Run 使用完全相同的 GUID / IOCTL / 4 字节缓冲。
  输出最后一行固定为 "RESULT:<OK|FAIL ...>" 供 Rust 解析。
.PARAMETER Mode
  plug = 插入一块；unplug = 拔出一块（FIFO，最早插入的先拔）
.PARAMETER Count
  重复执行次数，默认 1
#>
[CmdletBinding()]
param(
    [ValidateSet('plug', 'unplug')]
    [string]$Mode = 'plug',
    [int]$Count = 1
)

$ErrorActionPreference = 'Stop'

if (-not ('UsbMmiddPlug' -as [type])) {
Add-Type -TypeDefinition @"
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public class UsbMmiddPlug {
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    public static extern IntPtr SetupDiGetClassDevs(ref Guid classGuid, string enumerator, IntPtr hwndParent, uint flags);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
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

    public static string Run(uint cmd) {
        Guid g = new Guid("{B5FFD75F-DA40-4353-8FF8-B6DAF6F1D8CA}");
        IntPtr h = SetupDiGetClassDevs(ref g, null, IntPtr.Zero, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
        if (h == (IntPtr)(-1)) { int e0 = Marshal.GetLastWin32Error(); return "FAIL GetClassDevs err=" + e0 + " (" + Msg(e0) + ")"; }
        try {
            SP_DEVICE_INTERFACE_DATA ifd = new SP_DEVICE_INTERFACE_DATA();
            ifd.cbSize = (uint)Marshal.SizeOf(typeof(SP_DEVICE_INTERFACE_DATA));
            if (!SetupDiEnumDeviceInterfaces(h, IntPtr.Zero, ref g, 0, ref ifd)) {
                int e = Marshal.GetLastWin32Error();
                return (e == 259) ? "NO_INTERFACE" : "FAIL EnumInterfaces err=" + e + " (" + Msg(e) + ")";
            }
            uint req = 0;
            SetupDiGetDeviceInterfaceDetail(h, ref ifd, IntPtr.Zero, 0, out req, IntPtr.Zero);
            if (req == 0) {
                int e = Marshal.GetLastWin32Error();
                return "FAIL GetDetail probe err=" + e + " (" + Msg(e) + ") req=0";
            }
            IntPtr buf = Marshal.AllocHGlobal((int)req);
            try {
                // cbSize 按位数：x64=8, x86=6
                Marshal.WriteInt32(buf, 0, (IntPtr.Size == 8) ? 8 : 6);
                if (!SetupDiGetDeviceInterfaceDetail(h, ref ifd, buf, req, out req, IntPtr.Zero)) {
                    int e = Marshal.GetLastWin32Error(); return "FAIL GetDetail err=" + e + " (" + Msg(e) + ")";
                }
                string path = Marshal.PtrToStringUni(new IntPtr(buf.ToInt64() + 4));
                if (string.IsNullOrEmpty(path)) return "FAIL empty device path";
                IntPtr hd = CreateFile(path, GENERIC_READ | GENERIC_WRITE, 0, IntPtr.Zero, OPEN_EXISTING, 0, IntPtr.Zero);
                if (hd == (IntPtr)(-1) || hd == IntPtr.Zero) { int e = Marshal.GetLastWin32Error(); return "FAIL CreateFile err=" + e + " (" + Msg(e) + ") path=" + path; }
                try {
                    byte[] inb = new byte[4] { (byte)cmd, 0, 0, 0 };
                    uint ret;
                    if (!DeviceIoControl(hd, IOCTL_PLUG, inb, 4, null, 0, out ret, IntPtr.Zero)) {
                        int e = Marshal.GetLastWin32Error(); return "FAIL IoControl err=" + e + " (" + Msg(e) + ")";
                    }
                    return "IOCTL_OK cmd=0x" + cmd.ToString("X") + " path=" + path;
                } finally { CloseHandle(hd); }
            } finally { Marshal.FreeHGlobal(buf); }
        } finally { SetupDiDestroyDeviceInfoList(h); }
    }
}
"@
}

$cmd = if ($Mode -eq 'plug') { 0x10 } else { 0x00 }
Write-Output "arch=x$(if ([Environment]::Is64BitProcess) {64} else {86}) cbSize=$(if ([IntPtr]::Size -eq 8) {8} else {6}) mode=$Mode count=$Count"

$fail = 0
for ($i = 0; $i -lt $Count; $i++) {
    $r = [UsbMmiddPlug]::Run($cmd)
    Write-Output "step=$($i + 1) $r"
    if ($r -like 'IOCTL_OK*') { Start-Sleep -Milliseconds 400 } else { $fail++ }
}

if ($fail -eq $Count) { Write-Output "RESULT:FAIL $Mode 全部失败" ; exit 2 }
Write-Output "RESULT:OK $Mode 成功 $($Count - $fail)/$Count"
exit 0
