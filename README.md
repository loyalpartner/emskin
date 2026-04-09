# eafvil

嵌套 Wayland 合成器，为 [Emacs Application Framework](https://github.com/emacs-eaf/emacs-application-framework) 提供原生 Wayland 应用嵌入能力。

eafvil 在一个 winit 窗口内运行独立的 Wayland 合成器，Emacs 作为主窗口全屏运行，EAF 应用作为子窗口叠加在指定区域。

## 特性

- Emacs 全屏嵌入，EAF 应用窗口由 Emacs 通过 IPC 控制位置和大小
- 窗口镜像 — 同一 EAF 应用可在多个 Emacs 窗口中显示（GPU 纹理共享，零拷贝）
- 主机剪贴板双向同步
- Popup 支持（右键菜单、下拉框等）
- xdg_activation_v1 焦点转移
- 通过 CLI 参数指定键盘布局（`--xkb-layout` 等）

## 依赖

- Rust 1.70+
- Wayland 开发库
- [smithay](https://github.com/Smithay/smithay)（自动从 Git 拉取）

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
cd eafvil
cargo build --release
```

编译产物位于 `eafvil/target/release/eafvil`。

代码检查：

```bash
cargo clippy -- -D warnings
cargo fmt --check
```

## 使用

### 直接启动

eafvil 默认会自动启动 Emacs：

```bash
./target/release/eafvil
```

### CLI 参数

```
eafvil [OPTIONS]

Options:
  --no-spawn              不启动 Emacs，等待外部连接
  --emacs-command <CMD>   Emacs 启动命令 (默认: "emacs")
  --ipc-path <PATH>       指定 IPC socket 路径 (默认: $XDG_RUNTIME_DIR/eafvil-<pid>.ipc)
  --xkb-layout <LAYOUT>   键盘布局 (例: "us", "cn")
  --xkb-model <MODEL>     键盘型号 (例: "pc105")
  --xkb-variant <VAR>     布局变体 (例: "nodeadkeys")
  --xkb-options <OPTS>    XKB 选项 (例: "ctrl:nocaps")
```

### Emacs 集成

加载 Elisp 客户端：

```elisp
(add-to-list 'load-path "/path/to/eafvil/elisp")
(require 'eaf-eafvil)
```

客户端会通过父进程 PID 自动发现 IPC socket 并连接。连接后 Emacs 窗口大小变化会自动同步给合成器。

## 项目结构

```
eafvil/         Rust 嵌套 Wayland 合成器
  src/
    main.rs       入口，CLI 解析，事件循环
    state.rs      合成器状态
    apps.rs       EAF 应用窗口管理
    input.rs      键盘/鼠标输入处理
    clipboard.rs  剪贴板同步
    winit.rs      winit 窗口后端
    handlers/     Wayland 协议处理 (xdg_shell, compositor, xdg_activation)
    ipc/          IPC 通信 (长度前缀 JSON over Unix socket)
    grabs/        移动/调整大小 (预留)
elisp/          Emacs IPC 客户端
demo/           演示应用
```

## IPC 协议

Emacs 与合成器通过 Unix socket 通信，使用长度前缀 JSON 协议。

**Emacs -> 合成器:** `set_geometry`, `close`, `set_visibility`, `forward_key`, `add_mirror`, `update_mirror_geometry`, `remove_mirror`, `promote_mirror`

**合成器 -> Emacs:** `connected`, `surface_size`, `window_created`, `window_destroyed`, `title_changed`

## License

GPL-3.0
