# 虚拟显示驱动（可选）

本目录只提交 Rebleach 自己编写的安装 / 插拔脚本：

| 文件 | 说明 |
|---|---|
| `Install-VirtualDisplay.ps1` | 安装并创建虚拟显示设备，需管理员权限运行 |
| `PlugMonitor.ps1` | 只做插 / 拔虚拟显示器（usbmmidd `IOCTL_PLUG`），供 Agent 调用 |

驱动本体（`usbmmidd.inf` / `usbmmidd.cat` / `x64/usbmmIdd.dll`）属于第三方软件，
**不随仓库分发**。需要「无物理显示器也能远程」或「远程增删虚拟屏」时，
请自行获取 usbmmidd 驱动并放到本目录，保持以下结构：

```
virtual-display/
├── Install-VirtualDisplay.ps1
├── PlugMonitor.ps1
├── usbmmidd.inf
├── usbmmidd.cat
└── x64/
    └── usbmmIdd.dll
```

放好后重新构建 Agent 安装包，NSIS 安装流程会自动以管理员权限执行
`Install-VirtualDisplay.ps1` 完成驱动入库与虚拟设备创建。
