<p align="center">
  <img src="images/banner.png" alt="emskin — Emacs dressed in a Wayland compositor" width="720"/>
</p>

# emskin

> 给 Emacs 加上皮肤 — Dress Emacs in a Wayland skin.

emskin 把 Emacs 放进一个 Wayland 合成器里，让**任意程序**（浏览器、终端、视频播放器等）都能像原生 buffer 一样嵌入 Emacs 窗口。

![demo](screenshots/demo.gif)

## 特性

- **任意程序嵌入** — Wayland 和 X11 程序均可嵌入
- **窗口镜像** — 同一程序显示在多个 Emacs 窗口
- **输入法支持** — CJK 输入法开箱即用
- **剪贴板同步** — 主机与嵌入程序双向同步
- **启动器支持** — rofi / wofi 等可直接使用
- **自动焦点管理** — 新窗口自动获焦，关闭后自动回退

## 兼容性

| 桌面环境 | Wayland | X11 |
|----------|---------|-----|
| GNOME    | ✓       | ✓   |
| KDE      | ✓       | ✓   |
| Sway     | ✓       | —   |
| COSMIC   | ✓       | —   |

推荐 pgtk Emacs（`--with-pgtk`），GTK3 X11 版本亦可通过 XWayland 运行。

## 快速开始

```bash
# 安装依赖（Arch Linux）
sudo pacman -S wayland libxkbcommon mesa

# 编译
cd emskin && cargo build --release

# 启动（自动运行 Emacs）
./target/release/emskin

# 或零配置独立模式
./target/release/emskin --standalone
```

## Emacs 配置

```elisp
(add-to-list 'load-path "/path/to/emskin/elisp")
(require 'emskin)
```

使用 `--standalone` 模式则无需配置。

## 启动嵌入程序

```
M-x emskin-open-native-app RET firefox
M-x emskin-open-native-app RET foot
```

也可以绑定快捷键启动 rofi 等启动器：

```elisp
(defun my/emskin-rofi ()
  (interactive)
  (start-process "rofi" nil
                 "setsid" "rofi"
                 "-show" "combi"
                 "-combi-modi" "drun,ssh"
                 "-terminal" "foot"
                 "-show-icons" "-i"))
(global-set-key (kbd "C-c r") #'my/emskin-rofi)
```

## CLI 参数

```
emskin [OPTIONS]

  --no-spawn              不启动 Emacs，等待外部连接
  --command <CMD>         启动命令 (默认: "emacs")
  --arg <ARG>             命令参数 (可多次指定)
  --standalone            独立模式，自动加载内置 elisp
  --xkb-layout <LAYOUT>   键盘布局 (例: "us", "cn")
```

## License

GPL-3.0
