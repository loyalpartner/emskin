# IME 输入法设计

## 问题

emskin 是嵌套 Wayland 合成器，内部运行的程序有两类 IME 路径：

- **纯 Wayland 客户端**（Chrome Ozone/Wayland）：通过 `zwp_text_input_v3` 协议与合成器通信
- **GTK/Qt 客户端**（Firefox、Emacs）：通过 IM 模块（fcitx5-gtk）经 DBus 直连 IME 守护进程

两条路径必须同时工作，互不干扰。

## 架构总览

```
┌───────────────────────────────────────────┐
│ 宿主合成器                                 │
│                                           │
│  fcitx5 ◄── input_method_v2 ──► 宿主      │
│    │                              │       │
│    │ DBus                   text_input_v3  │
│    │                              │       │
│    ▼                              ▼       │
│  ┌──────────────────────────────────┐     │
│  │ emskin (winit 窗口)              │     │
│  │                                  │     │
│  │  winit::Ime ──桥接──► text_input │     │
│  │                        (服务端)  │     │
│  │       │                    │     │     │
│  │       │                    ▼     │     │
│  │       │               Chrome    │     │
│  │       │            (纯 Wayland)  │     │
│  │       │                         │     │
│  │       └── DBus ──► Firefox      │     │
│  │                  (fcitx5-gtk)    │     │
│  └──────────────────────────────────┘     │
└───────────────────────────────────────────┘
```

## 核心思路

### 1. 桥接而非内嵌 IME

宿主已经运行 fcitx5/ibus，emskin 不需要自己跑 IME 实例，也不需要实现 `input_method_v2` 协议。只需：

- 服务端注册 `text_input_v3`（让纯 Wayland 客户端能绑定）
- 收到 winit 的 `Ime::Preedit/Commit` 事件时，转发给焦点客户端

### 2. 按客户端类型切换宿主 IME

关键发现：注册 `text_input_v3` 全局协议后，fcitx5-gtk 会自动从 DBus 切换到 text_input 路径。如果同时让宿主 fcitx5 拦截按键（`set_ime_allowed(true)`），GTK 程序会出现双重处理导致 IME 失效。

解决：焦点切换时检查客户端是否绑定了 `text_input_v3`：
- **绑了**（Chrome）→ 启用宿主 IME → 按键经 fcitx5 → winit Ime 事件 → 桥接到客户端
- **没绑**（Firefox）→ 禁用宿主 IME → 按键正常通过 wl_keyboard → fcitx5-gtk 自行处理

### 3. 手动管理 text_input 焦点

smithay 的 `text_input.enter()/leave()` 被门控在 `input_method.has_instance()` 之后。emskin 没有运行 input_method，所以需要在 `focus_changed` 回调中手动发送 enter/leave。

注意：`focus_changed` 被调用时 smithay 已经更新了 text_input 的内部焦点，需要临时交换焦点才能把 leave 发给正确的旧客户端。

### 4. 延迟应用 `set_ime_allowed`

`focus_changed` 在 smithay 回调链中被调用，此时无法访问 winit backend。通过 `pending_ime_allowed` 标志延迟到事件循环中应用，与 `pending_fullscreen`/`pending_maximize` 使用相同模式。

## smithay 补丁

fork: `loyalpartner/smithay` 分支 `emskin-patches`

| 改动 | 原因 |
|------|------|
| 暴露 `WinitEvent::Ime` | 原代码静默丢弃 IME 事件 |
| 移除 text_input 的 `has_instance()` 守卫 | 允许无 input_method 时处理 text_input 请求 |
| 暴露 `cursor_rectangle()` 访问器 | 让合成器读取客户端光标位置并同步给宿主 |

## 限制

- XWayland 程序通过 XIM 处理输入法，不走 text_input_v3
- 光标位置同步依赖客户端正确上报 `set_cursor_rectangle`
- 如需在 emskin 内运行独立 IME 实例，需额外实现 `input_method_v2` 协议
