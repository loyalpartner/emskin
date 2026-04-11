<p align="center">
  <img src="images/banner.png" alt="emskin — Emacs dressed in a Wayland compositor" width="720"/>
</p>

# emskin

> 给 Emacs 加上皮肤 — Dress Emacs in a Wayland skin.

emskin 是一个嵌套 Wayland 合成器：在一个 winit 窗口内运行独立的 Wayland/XWayland 合成器，Emacs 作为主窗口全屏铺满，**任意 Wayland 或 X11 程序**（浏览器、终端、视频播放器、PDF 阅读器、IDE 等）都可以作为子窗口，由 Emacs 通过 IPC 精确控制位置、大小、可见性和焦点——就像这些程序被原生嵌入到 Emacs 里一样。

它最初为 [Emacs Application Framework](https://github.com/emacs-eaf/emacs-application-framework) 的 Wayland 后端而设计，但不绑定 EAF：`emskin-open-native-app` 可以把任何外部程序当作一块"嵌入式 buffer"挂到当前 Emacs 窗口里。

> **迁移提示**（原 `eafvil` 用户）：本项目已从 `eafvil` 更名为 `emskin`。请将 `(require 'eaf-eafvil)` 改为 `(require 'emskin)`，`load-path` 指向 `…/emskin/elisp`，并重新编译二进制（路径和名称均已变更）。公开命令 `eaf-open-app` / `eaf-open-native-app` 分别重命名为 `emskin-open-app` / `emskin-open-native-app`。

![demo](screenshots/demo.gif)

## 特性

- **任意程序嵌入** — 原生 Wayland 与 XWayland 双协议支持，可承载 GTK / Qt / Electron / X11 程序
- **Emacs 控制几何** — 每个嵌入窗口的位置、大小、可见性、焦点均由 Emacs 通过 IPC 精确控制
- **窗口镜像** — 同一个程序可在多个 Emacs 窗口中显示（GPU 纹理共享，零拷贝）
- **中文/日文/韩文输入法** — 纯 Wayland 客户端（Chrome）通过 text_input_v3 桥接宿主 IME；GTK/Qt 客户端（Firefox）通过 fcitx5-gtk 直连，两条路径自动切换互不干扰
- **主机剪贴板双向同步** — pgtk Emacs 通过 Wayland data_device，GTK3 Emacs 通过 XWayland selection 桥接，两条路径自动路由
- **GPU 缓冲区共享** — linux-dmabuf 协议支持硬件加速客户端
- **光标形状跟随** — 嵌入程序的光标形状自动转发到宿主窗口（链接显示手指、文本框显示 I-beam 等）。Wayland 客户端通过 wp_cursor_shape_v1，X11 客户端通过 XFixes cursor 追踪
- **Popup 支持**（右键菜单、下拉框、补全浮层等）
- **自动焦点管理** — 新启动的应用自动获取焦点，关闭窗口后焦点自动回退
- **通过 CLI 参数指定键盘布局**（`--xkb-layout` 等）

> **推荐使用 pgtk (pure GTK) 版本的 Emacs**（`--with-pgtk` 编译），体验最佳。GTK3 X11 版本通过 XWayland 支持，包括全屏、嵌入应用、剪贴板同步、光标形状跟随等核心功能，但窗口几何计算可能有偏差。

## 兼容性

| 桌面环境 | Wayland | X11 |
|----------|---------|-----|
| GNOME    | ✓       | ✓   |
| KDE      | ✓       | ✓   |
| Sway     | ✓       | —   |
| COSMIC   | ✓       | —   |

Emacs 版本：pgtk（推荐）和 GTK3 X11（通过 XWayland）均支持。

## 依赖

- Rust 1.70+
- Wayland 开发库
- Emacs（推荐 pgtk 版本，GTK3 X11 版本亦可通过 XWayland 运行）
- [smithay](https://github.com/loyalpartner/smithay)（fork，自动从 Git 拉取）

Arch Linux:

```bash
sudo pacman -S wayland libxkbcommon mesa
```

Debian/Ubuntu:

```bash
sudo apt install libwayland-dev libxkbcommon-dev libegl-dev
```

## 编译

```bash
cd emskin
cargo build --release
```

编译产物位于 `emskin/target/release/emskin`。

代码检查：

```bash
cargo clippy -- -D warnings
cargo fmt --check
```

## 使用

### 直接启动

emskin 默认会自动启动 Emacs：

```bash
./target/release/emskin
```

### CLI 参数

```
emskin [OPTIONS]

Options:
  --no-spawn              不启动 Emacs，等待外部连接
  --command <CMD>         启动命令 (默认: "emacs")
  --arg <ARG>             命令参数 (可多次指定)
  --standalone            独立模式：自动加载内置 elisp，无需用户配置
  --ipc-path <PATH>       指定 IPC socket 路径 (默认: $XDG_RUNTIME_DIR/emskin-<pid>.ipc)
  --xkb-layout <LAYOUT>   键盘布局 (例: "us", "cn")
  --xkb-model <MODEL>     键盘型号 (例: "pc105")
  --xkb-variant <VAR>     布局变体 (例: "nodeadkeys")
  --xkb-options <OPTS>    XKB 选项 (例: "ctrl:nocaps")
```

### Emacs 集成

**方式一：独立模式（零配置）**

使用 `--standalone` 参数，emskin 会自动将内置的 elisp 文件注入 Emacs，无需修改 init.el：

```bash
./target/release/emskin --standalone
```

**方式二：手动配置**

在 Emacs init.el 中加载 elisp 客户端：

```elisp
(add-to-list 'load-path "/path/to/emskin/elisp")
(require 'emskin)
```

客户端会通过父进程 PID 自动发现 IPC socket 并连接。连接后 Emacs 窗口大小变化会自动同步给合成器。

### 启动应用

在 Emacs 中通过 `M-x` 调用：

**Demo 应用**（`demo/` 目录下的 Python PyQt6 应用）：

```
M-x emskin-open-app RET demo      — 交互式 Demo（文本输入、按钮、键盘事件）
M-x emskin-open-app RET caliper   — 几何对齐调试工具（边框标尺、坐标标签）
```

**原生应用**（任意 Wayland/X11 程序）：

```
M-x emskin-open-native-app RET firefox     — 启动 Firefox
M-x emskin-open-native-app RET foot        — 启动 foot 终端
M-x emskin-open-native-app RET mpv foo.mp4 — 启动 mpv 播放视频
```

应用启动后会自动获取焦点，窗口位置由 Emacs 通过 IPC 控制。

**启动器**（rofi/wofi 等 wlr-layer-shell 应用）：

emskin 支持 wlr-layer-shell 协议，可以直接运行 rofi/wofi 等启动器。Emacs 内部运行在 emskin 中，`start-process` 启动的程序自动连接到 emskin，无需额外配置。

在 init.el 中绑定快捷键：

```elisp
(defun my/emskin-rofi ()
  "Launch rofi application launcher."
  (interactive)
  (start-process "rofi" nil
                 "setsid" "rofi"
                 "-combi-modi" "drun,ssh"
                 "-show" "combi"
                 "-terminal" "foot"
                 "-font" "JetBrainsMono Nerd Font"
                 "-show-icons" "-i"))
(global-set-key (kbd "C-c r") #'my/emskin-rofi)
```

> **提示**：使用 `setsid` 让 rofi 在独立会话中运行，确保从 rofi 启动的程序不受 Emacs 进程组影响。

## 项目结构

```
emskin/         Rust 嵌套 Wayland 合成器
  src/
    main.rs       入口，CLI 解析，事件循环
    state.rs      合成器状态
    apps.rs       嵌入程序窗口管理
    input.rs      键盘/鼠标输入处理
    clipboard.rs  剪贴板同步
    winit.rs      winit 窗口后端
    handlers/     Wayland 协议处理 (xdg_shell, compositor, xwayland)
    ipc/          IPC 通信 (长度前缀 JSON over Unix socket)
    grabs/        移动/调整大小 (预留)
elisp/          Emacs IPC 客户端
demo/           演示应用
```

## IPC 协议

Emacs 与合成器通过 Unix socket 通信，使用长度前缀 JSON 协议。

**Emacs -> 合成器:** `set_geometry`, `close`, `set_visibility`, `prefix_done`, `set_focus`, `set_crosshair`, `add_mirror`, `update_mirror_geometry`, `remove_mirror`, `promote_mirror`

**合成器 -> Emacs:** `connected`, `surface_size`, `window_created`, `window_destroyed`, `title_changed`, `focus_view`, `xwayland_ready`

## License

GPL-3.0
