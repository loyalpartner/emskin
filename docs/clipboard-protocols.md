# 剪切板协议时序参考

本文档只讲协议本身，不涉及任何具体合成器实现。

## 目录

1. [X11 ICCCM selection（含 XFixes / INCR）](#1-x11-icccm-selection含-xfixes--incr)
2. [data_control（wlr / ext，无焦点 Wayland 剪切板）](#2-data_controlwlr--ext无焦点-wayland-剪切板)
3. [wl_data_device（带焦点 Wayland 剪切板 / DnD）](#3-wl_data_device带焦点-wayland-剪切板--dnd)
4. [XWayland 选择桥](#4-xwayland-选择桥)
5. [参考协议链接](#参考协议链接)

## 角色术语

- **Owner / Source owner**：持有剪切板内容的客户端
- **Requestor**：想要读剪切板的客户端
- **Compositor / X server**：中介，持有"谁是 owner"这个事实，转发请求

---

## 1. X11 ICCCM selection（含 XFixes / INCR）

X 没有"剪切板守护进程"，剪切板是三个命名的 *selection*：`PRIMARY`、`SECONDARY`、`CLIPBOARD`。内容保存在 owner 进程内存里，通过 *property* 机制拷贝给 requestor。

关键原语：
- `SetSelectionOwner(sel, window, time)` — 声明自己是 owner
- `ConvertSelection(sel, target, property, requestor, time)` — 请求数据
- `SelectionRequest` 事件 — 服务器把请求投递给 owner
- `SelectionNotify` 事件 — owner 回执给 requestor
- `GetProperty` / `ChangeProperty` — 通过窗口属性搬数据
- XFixes `SelectSelectionInput` — 订阅 owner 变更通知

### 1.1 复制（声明 owner）

```
Owner                                 X server
  │                                      │
  │ SetSelectionOwner(CLIPBOARD, self)   │
  ├─────────────────────────────────────►│
  │                                      │
  │ （之后订阅 XFixes 的其他客户端会收到 │
  │   XFixesSelectionNotify 通知）       │
  │                                      │
  │ ← 至此 owner 只需等待 SelectionRequest
```

### 1.2 粘贴（两步：先问 TARGETS，再取数据）

```
Requestor                  X server                  Owner
    │                          │                        │
    │ (1) 查询 MIME 列表                                │
    │ ConvertSelection(        │                        │
    │  sel=CLIPBOARD,          │                        │
    │  target=TARGETS,         │                        │
    │  property=_MY_TARGETS,   │                        │
    │  requestor=self)         │                        │
    ├─────────────────────────►│                        │
    │                          │ SelectionRequest       │
    │                          │ (target=TARGETS,       │
    │                          │  property=_MY_TARGETS) │
    │                          ├───────────────────────►│
    │                          │                        │
    │                          │                        │ ChangeProperty(
    │                          │                        │  requestor,
    │                          │                        │  _MY_TARGETS,
    │                          │                        │  ATOM,
    │                          │                        │  [TARGETS, UTF8_STRING, text/html, ...])
    │                          │◄───────────────────────┤
    │                          │                        │
    │                          │                        │ SendEvent(SelectionNotify,
    │                          │                        │           requestor,
    │                          │                        │           property=_MY_TARGETS)
    │                          │◄───────────────────────┤
    │ SelectionNotify          │                        │
    │◄─────────────────────────┤                        │
    │                          │                        │
    │ GetProperty(             │                        │
    │  _MY_TARGETS,            │                        │
    │  delete=true)            │                        │
    ├─────────────────────────►│                        │
    │ ← atom list              │                        │
    │                          │                        │
    │ (2) 根据 atom list 选一个 target，取实际数据      │
    │ ConvertSelection(        │                        │
    │  sel=CLIPBOARD,          │                        │
    │  target=UTF8_STRING,     │                        │
    │  property=_MY_DATA)      │                        │
    ├─────────────────────────►│                        │
    │                          │ SelectionRequest       │
    │                          ├───────────────────────►│
    │                          │                        │
    │                          │                        │ ChangeProperty(
    │                          │                        │  _MY_DATA,
    │                          │                        │  UTF8_STRING,
    │                          │                        │  <bytes>)
    │                          │◄───────────────────────┤
    │                          │ SelectionNotify        │
    │                          │◄───────────────────────┤
    │ SelectionNotify          │                        │
    │◄─────────────────────────┤                        │
    │ GetProperty(_MY_DATA,    │                        │
    │             delete=true) │                        │
    ├─────────────────────────►│                        │
    │ ← bytes                  │                        │
```

转换失败时，owner 回 `SelectionNotify` 但 `property=None`。

### 1.3 INCR（大数据分块传输）

当数据超过 server 的 max request 大小（通常 ~256KB），owner 以 `INCR` 伪类型回应，requestor 通过 `PropertyNotify(NEW_VALUE)` 逐块拉取；**0 字节属性 = 结束**。

```
Requestor                 X server                 Owner
    │                         │                       │
    │ ConvertSelection(data)  │                       │
    ├────────────────────────►│                       │
    │                         │ SelectionRequest      │
    │                         ├──────────────────────►│
    │                         │                       │
    │                         │                       │ （数据太大，改走 INCR）
    │                         │                       │ ChangeProperty(
    │                         │                       │   prop, INCR, <total_size>)
    │                         │◄──────────────────────┤
    │                         │ SelectionNotify       │
    │                         │◄──────────────────────┤
    │ SelectionNotify         │                       │
    │◄────────────────────────┤                       │
    │                         │                       │
    │ GetProperty(prop,       │                       │
    │             delete=true)│                       │
    ├────────────────────────►│                       │
    │ ← type=INCR, size       │                       │
    │                         │                       │
    │ ── 循环开始 ──                                  │
    │                         │ PropertyNotify(DELETE)│
    │                         ├──────────────────────►│
    │                         │                       │ ChangeProperty(
    │                         │                       │   prop, <chunk>)
    │                         │◄──────────────────────┤
    │ PropertyNotify(         │                       │
    │   NEW_VALUE)            │                       │
    │◄────────────────────────┤                       │
    │ GetProperty(prop,       │                       │
    │             delete=true)│                       │
    ├────────────────────────►│                       │
    │ ← chunk                 │                       │
    │                         │ PropertyNotify(DELETE)│
    │                         ├──────────────────────►│
    │ ── 重复直到 owner 发送 0 字节 chunk 表示 EOF ── │
    │                         │                       │ ChangeProperty(
    │                         │                       │   prop, <empty>)
    │                         │◄──────────────────────┤
    │ PropertyNotify(NEW_VALUE)                       │
    │◄────────────────────────┤                       │
    │ GetProperty → 0 bytes   │                       │
    ├────────────────────────►│                       │
```

### 1.4 XFixes 监听 owner 变化

没有 XFixes 的年代要靠轮询。现在：

```
Client A (监听方)                X server
    │                                │
    │ XFixesSelectSelectionInput(    │
    │   window,                      │
    │   sel=CLIPBOARD,               │
    │   mask=SET_SELECTION_OWNER |   │
    │        SELECTION_WINDOW_DESTROY│
    │        SELECTION_CLIENT_CLOSE) │
    ├───────────────────────────────►│
    │                                │
    │ (Owner B 调用 SetSelectionOwner 时)
    │                                │
    │ XFixesSelectionNotify(         │
    │   selection=CLIPBOARD,         │
    │   owner=B,                     │
    │   timestamp)                   │
    │◄───────────────────────────────┤
```

---

## 2. data_control（wlr / ext，无焦点 Wayland 剪切板）

Wayland 核心协议里的 `wl_data_device` 是**焦点门控**的——客户端只有在窗口获得键盘焦点时才能读写剪切板。对剪切板管理器、截图工具、远程桌面这类 "后台服务" 来说不可用。

两个替代协议解决该问题（语义几乎一致，接口重名）：
- `zwlr_data_control_manager_v1` — wlroots 生态（sway / Hyprland / niri / cosmic）
- `ext_data_control_v1` — upstream 重新标准化，KDE Plasma 6.2+ 提供

三个对象：
- `*_manager` — 工厂，全局 singleton
- `*_device`  — 每个 `wl_seat` 一个，收 `data_offer` / `selection` / `primary_selection` 事件
- `*_source`  — 客户端创建，用来声明 "我要拥有选择"
- `*_offer`   — compositor 创建，代表其他客户端的选择

### 2.1 复制（声明选择）

```
Source client          Compositor        其他 data_control clients (A, B, ...)
      │                    │                            │
      │ manager.           │                            │
      │ create_data_source │                            │
      ├───────────────────►│                            │
      │                    │                            │
      │ source.offer(      │                            │
      │   "text/plain;utf-8")                           │
      │ source.offer(      │                            │
      │   "text/html")     │                            │
      ├───────────────────►│                            │
      │                    │                            │
      │ device.            │                            │
      │ set_selection(     │                            │
      │   source)          │                            │
      ├───────────────────►│                            │
      │                    │                            │
      │                    │ device.data_offer(new id)  │
      │                    ├───────────────────────────►│
      │                    │ offer.offer("text/plain…") │
      │                    ├───────────────────────────►│
      │                    │ offer.offer("text/html")   │
      │                    ├───────────────────────────►│
      │                    │ device.selection(offer)    │
      │                    ├───────────────────────────►│
```

Compositor 不复制数据，只广播 "当前选择换成这个 offer 了"。

### 2.2 粘贴（拉数据）

```
Requestor           Compositor                 Source owner
    │                   │                           │
    │ offer.receive(    │                           │
    │   "text/plain;utf-8",                         │
    │   fd=<write end>) │                           │
    ├──────────────────►│                           │
    │                   │ source.send(              │
    │                   │   "text/plain;utf-8",     │
    │                   │   fd=<same write end>)    │
    │                   ├──────────────────────────►│
    │                   │                           │
    │                   │                           │ write(fd, bytes…)
    │                   │                           │ close(fd)
    │ read(fd) ← bytes──┼───────────────────────────┤
    │ (EOF 即 EOF close)│                           │
```

要点：
- `offer.receive` 和 `source.send` 传递**同一个** pipe fd，compositor 只当搬运工。
- 可以多次 `receive` 不同 mime（要几个就 receive 几次）。
- 数据流是**单向** pipe，没有 ack，读方靠 close 感知 EOF。

### 2.3 对象销毁 / 取消

```
Source client                Compositor
      │                          │
      │ （另一客户端抢走选择）  │
      │                          │ source.cancelled
      │◄─────────────────────────┤
      │ source.destroy           │
      ├─────────────────────────►│

其他 data_control clients
      │ device.selection(new offer or null)
      │◄─────────────────────────┤
      │ 之前的 offer 不再被 send，应 offer.destroy
```

---

## 3. wl_data_device（带焦点 Wayland 剪切板 / DnD）

核心 Wayland 协议的剪切板 + 拖放通道。语义上和 data_control 近似，**但焦点门控**：
- `set_selection(source, serial)` 必须带一个来自输入事件（键盘 enter / key / button）的 serial，compositor 据此判断客户端确实在响应用户动作。
- 客户端只在自己的 surface 获得键盘焦点时，才会收到 `data_offer` / `selection` 事件。

额外：同一协议同时服务**拖放**——`start_drag` / `enter` / `leave` / `motion` / `drop` / `finish` 事件流；剪切板只用 `selection` 一条。

### 3.1 复制

```
Source client          Compositor           焦点在另一窗口的 client
      │                    │                          │
      │ (先收到 keyboard.enter → serial=S)            │
      │◄───────────────────┤                          │
      │                    │                          │
      │ manager.           │                          │
      │ create_data_source │                          │
      ├───────────────────►│                          │
      │ source.offer(mime) │                          │
      ├───────────────────►│                          │
      │ device.            │                          │
      │ set_selection(     │                          │
      │   source, S)       │                          │
      ├───────────────────►│                          │
      │                    │ （compositor 把 S 对照   │
      │                    │   自己发出的最新 serial，│
      │                    │   不匹配 → 协议错误）    │
      │                    │                          │
      │                    │ 先前焦点的 client        │
      │                    │                          │
      │                    │ device.data_offer(id)    │
      │                    ├─────────────────────────►│
      │                    │ offer.offer(mime) × N    │
      │                    ├─────────────────────────►│
      │                    │ device.selection(offer)  │
      │                    ├─────────────────────────►│
```

**焦点切换时**会重新广播 `selection` 事件给新焦点的客户端（事件流是 per-focus 的，不是 per-selection 的）。

### 3.2 粘贴

```
Focused client      Compositor              Source owner
      │                  │                         │
      │ offer.receive(   │                         │
      │   mime, fd)      │                         │
      ├─────────────────►│                         │
      │                  │ source.send(mime, fd)   │
      │                  ├────────────────────────►│
      │                  │                         │ write(fd); close(fd)
      │ read(fd)◄────────┼─────────────────────────┤
```

pipe 语义和 data_control 相同。

### 3.3 primary selection（中键粘贴）

同族协议 `zwp_primary_selection_device_manager_v1`（unstable） / `ext_primary_selection_v1`（staging）。对象名换成 `primary_selection_{device,source,offer}`，语义一致但事件独立：`primary_selection` 事件而非 `selection`。

---

## 4. XWayland 选择桥

XWayland = "在 Wayland 合成器里跑一个翻译版 X server"。传统 X 客户端的 SetSelectionOwner / ConvertSelection 请求必须能跟 Wayland 客户端的 `wl_data_device` / `data_control` 相互通。

由 **Xwm**（合成器侧的 X window manager，smithay / wlroots / Mutter 各有实现）承担双向代理。Xwm 有两个自己的 X 窗口（通常一个给 CLIPBOARD 一个给 PRIMARY）作为 "X 世界里的代理 owner"。

### 4.1 X client 复制 → Wayland client 粘贴

```
X client          X server (XWayland)         Xwm           Compositor       Wayland client
   │                    │                      │                │                   │
   │ SetSelectionOwner  │                      │                │                   │
   │ (CLIPBOARD, self)  │                      │                │                   │
   ├───────────────────►│                      │                │                   │
   │                    │ XFixesSelectionNotify│                │                   │
   │                    ├─────────────────────►│                │                   │
   │                    │                      │ ConvertSelection(TARGETS) → X client
   │                    │◄─────────────────────┤                │                   │
   │ SelectionRequest   │                      │                │                   │
   │◄───────────────────┤                      │                │                   │
   │ ChangeProperty +   │                      │                │                   │
   │ SelectionNotify    │                      │                │                   │
   ├───────────────────►├─────────────────────►│                │                   │
   │                    │                      │ atoms→mimes    │                   │
   │                    │                      │                │                   │
   │                    │                      │ 在 Wayland 侧以 Xwm 为 source owner│
   │                    │                      │ 调 data_control_manager /         │
   │                    │                      │ wl_data_device_manager            │
   │                    │                      │ create_data_source + offer(mime)  │
   │                    │                      │ + set_selection                   │
   │                    │                      ├───────────────►│                   │
   │                    │                      │                │ data_offer        │
   │                    │                      │                ├──────────────────►│
   │                    │                      │                │ selection(offer)  │
   │                    │                      │                ├──────────────────►│

  ── 粘贴时：Wayland client 向 compositor 发 offer.receive(mime, fd) ──

   │                    │                      │ source.send(mime, fd) 回到 Xwm
   │                    │                      │◄───────────────┤                   │
   │                    │                      │ 以 Xwm 的内部 X 代理窗口作为      │
   │                    │                      │ requestor，发起                    │
   │                    │                      │ ConvertSelection(mime, prop)       │
   │                    │                      ├─────────────────────►│             │
   │                    │ SelectionRequest     │                      │             │
   │                    │◄─────────────────────┤                      │             │
   │ SelectionRequest   │                      │                      │             │
   │◄───────────────────┤                      │                      │             │
   │ ChangeProperty +   │                      │                      │             │
   │ SelectionNotify    │                      │                      │             │
   ├───────────────────►├─────────────────────►│                      │             │
   │                    │                      │ 读 property bytes →  │             │
   │                    │                      │ write(fd); close(fd) │             │
   │                    │                      ├─────────────────────────────────►│ read(fd)
```

大数据自动走 X 侧的 INCR（§1.3），Xwm 内部边读 chunk 边 write 到 pipe，Wayland 侧感知不到分块。

### 4.2 Wayland client 复制 → X client 粘贴

对称关系。Wayland client 调 `set_selection`；Xwm 在 X 侧代表它 `SetSelectionOwner(CLIPBOARD, xwm_proxy_window)`。

```
Wayland client   Compositor       Xwm         XWayland X server      X client
     │                │             │                 │                   │
     │ set_selection  │             │                 │                   │
     ├───────────────►│             │                 │                   │
     │                │ data_offer  │                 │                   │
     │                ├────────────►│                 │                   │
     │                │ selection   │                 │                   │
     │                ├────────────►│                 │                   │
     │                │             │                 │                   │
     │                │             │ SetSelectionOwner                   │
     │                │             │ (CLIPBOARD,     │                   │
     │                │             │  xwm_proxy_win) │                   │
     │                │             ├────────────────►│                   │
     │                │             │                 │ XFixesSelectionNotify
     │                │             │                 ├──────────────────►│ （订阅方收到）

  ── 粘贴时：X client 发起 ──

     │                │             │                 │ ConvertSelection  │
     │                │             │                 │ (TARGETS / mime)  │
     │                │             │                 │◄──────────────────┤
     │                │             │ SelectionRequest│                   │
     │                │             │◄────────────────┤                   │
     │                │             │                 │                   │
     │                │ offer.receive(mime, fd)       │                   │
     │                │◄────────────┤                 │                   │
     │ source.send    │             │                 │                   │
     │ (mime, fd)     │             │                 │                   │
     │◄───────────────┤             │                 │                   │
     │ write(fd);     │             │                 │                   │
     │ close(fd)      │             │                 │                   │
     ├───────────────►│ pipe bytes 回 Xmwm            │                   │
     │                │             │ ChangeProperty  │                   │
     │                │             │ (prop, bytes)   │                   │
     │                │             ├────────────────►│                   │
     │                │             │ SendEvent(      │                   │
     │                │             │   SelectionNotify)                  │
     │                │             ├────────────────►├──────────────────►│ 读数据
```

### 4.3 X↔X 粘贴不经过 Wayland

当 owner 和 requestor 都是 XWayland 下的 X client 时，请求在 X server 内部闭环，Xwm 只需要让出（不声明 X 侧 owner），甚至可能不介入。

---

## 参考协议链接

### Wayland

- **wayland.xml**（核心协议，含 `wl_data_device` / `wl_data_source` / `wl_data_offer` / `wl_data_device_manager`）
  <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
- **ext-data-control-v1**（staging，upstream 无焦点剪切板）
  <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/tree/main/staging/ext-data-control>
- **wlr-data-control-unstable-v1**（wlroots 生态）
  <https://gitlab.freedesktop.org/wlroots/wlr-protocols/-/blob/master/unstable/wlr-data-control-unstable-v1.xml>
- **primary-selection-unstable-v1**（中键粘贴，wl_data_device 家族）
  <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/primary-selection/primary-selection-unstable-v1.xml>
- **ext-primary-selection-v1**（upstream 标准化版本）
  <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/tree/main/staging/ext-primary-selection>

### X11

- **ICCCM** — *Inter-Client Communication Conventions Manual*（§2 selection 协议，INCR）
  <https://www.x.org/releases/X11R7.7/doc/xorg-docs/icccm/icccm.html>
- **Xlib §4.5 Selections**（`ConvertSelection` / `SendEvent(SelectionNotify)` / `GetProperty` / `ChangeProperty`）
  <https://www.x.org/releases/X11R7.7/doc/libX11/libX11/libX11.html>
- **XFixes extension**（`SelectSelectionInput` / `SelectionNotify` 事件）
  <https://www.x.org/releases/X11R7.7/doc/fixesproto/fixesproto.txt>
- **freedesktop.org clipboards spec**（`CLIPBOARD_MANAGER` / `SAVE_TARGETS`，让剪切板在 owner 退出后保留）
  <https://www.freedesktop.org/wiki/ClipboardManager/>

### XWayland 侧参考实现

- **Xwayland/xwayland-selection.c**（Xwayland 官方 selection 桥，最权威的行为参考）
  <https://gitlab.freedesktop.org/xorg/xserver/-/blob/master/hw/xwayland/xwayland-selection.c>
- **wlroots xwm**
  <https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/master/xwayland/selection/>
- **sway xwayland selection**
  <https://github.com/swaywm/sway/blob/master/sway/desktop/xwayland.c>
- **smithay `X11Wm` selection 钩子**（`XwmHandler::{new_selection, send_selection, cleared_selection}`）
  <https://docs.rs/smithay/latest/smithay/xwayland/xwm/trait.XwmHandler.html>

### 实用工具（读协议时拿来对照行为）

- **wl-clipboard** — `wl-copy` / `wl-paste`，wlr-data-control 最小示例
  <https://github.com/bugaevc/wl-clipboard>
- **xclip** / **xsel** — X11 selection 命令行
- **wayland-info** / **weston-info** — 看宿主暴露了哪些 data-control / data-device 全局
