# MacWindowResizer

自动调整 macOS 新建主窗口的尺寸和位置，保留后续手动调整。

应用启动时若辅助功能接口尚未就绪，会在 5 秒内每隔 200 毫秒重试，并补处理监听就绪前创建的主窗口。服务启动前已打开的窗口不会被补调；同一窗口只自动处理一次。

未指定坐标时，优先保留原位置；如果目标尺寸会超出当前屏幕可用区域，则将窗口移入可用区域后再调整尺寸。日志会读取并确认实际尺寸。

需要 macOS 13+、Rust stable 和 Xcode Command Line Tools。

编辑 `config.toml`，以应用 bundle ID 为节名，尺寸和坐标单位为逻辑点：

```toml
[com.apple.finder]
width = 860
height = 660
# x = 434
# y = 218
```

在项目根目录运行：

```sh
cargo enable   # 构建、安装、启动，并启用登录启动
cargo refresh  # 完整卸载后重新构建、安装并启动（修改配置后执行）
cargo disable  # 卸载应用和登录服务，重置授权并删除日志
```

首次运行需在「系统设置 → 隐私与安全性 → 辅助功能」中允许 **MacWindowResizer**；重新构建后可能需要再次授权。
