# emskin 剪切板流程

emskin 是嵌套 Wayland 合成器，需要在 **宿主**（X11 或 Wayland 桌面）与 **内部客户端**（Emacs、EAF、嵌入的 X11/Wayland app）之间双向桥接剪切板。依据宿主能力与 emskin 内部的 world（Wayland / X11），共有四条路径。

## 目录

1. [X11 剪切板（宿主是 X11）](#1-x11-剪切板宿主是-x11)
2. [data_control（宿主 Wayland，主路径）](#2-data_control宿主-wayland主路径)
3. [wl_data_device（宿主 Wayland，fallback）](#3-wl_data_device宿主-waylandfallback)
4. [XWayland 内部桥（内部 X ↔ 内部 Wayland）](#4-xwayland-内部桥内部-x--内部-wayland)
5. [参考协议 / 代码位置](#参考协议--代码位置)

## 角色约定

- **宿主 compositor / X server**：emskin 外部的桌面环境
- **emskin**：合成器本体，同时是宿主的"客户端"
- **smithay dd**：emskin 内部的 `smithay::wayland::selection::data_device` 总线
- **内部客户端**：跑在 emskin 里的 Wayland / X11 应用

---

## 1. X11 剪切板（宿主是 X11）

**文件**：`crates/emskin/src/clipboard_x11.rs`

### 要点

- 在根窗口下建 10×10 隐藏代理窗口，订阅 `XFixesSelectSelectionInput` 监听 owner 变化，避免轮询。
- MIME ↔ Atom 双向翻译（TEXT / UTF8_STRING 有 fast-path，其余 `InternAtom` / `GetAtomName`）。
- 大数据走 ICCCM INCR：`PropertyNotify(NEW_VALUE)` 累积，零字节块收尾。
- `suppress_{clipboard,primary}` 抑制我们自己 `SetSelectionOwner` 的 XFixes 回显，防自环。

### 外部 X 复制 → 内部 Wayland 粘贴

```
X owner         X server            emskin proxy win        smithay dd              内部 wl client
  │                │                      │                      │                         │
  │SetSelection    │                      │                      │                         │
  │Owner(CLIPBOARD)│                      │                      │                         │
  ├───────────────►│                      │                      │                         │
  │                │ XFixesSelectionNotify│                      │                         │
  │                ├─────────────────────►│ on_xfixes_notify     │                         │
  │                │ ConvertSelection     │                      │                         │
  │                │ (TARGETS)            │                      │                         │
  │                │◄─────────────────────┤                      │                         │
  │SelectionRequest│                      │                      │                         │
  │◄───────────────┤                      │                      │                         │
  │ChangeProperty  │                      │                      │                         │
  │(atom list)     │                      │                      │                         │
  ├───────────────►│                      │                      │                         │
  │SelectionNotify │                      │                      │                         │
  ├───────────────►├─────────────────────►│ on_selection_notify  │                         │
  │                │ GetProperty          │ handle_targets_reply │                         │
  │                │◄─────────────────────┤ atom → mime          │                         │
  │                │                      ├─────────────────────►│ set_data_device_        │
  │                │                      │                      │ selection(mimes)        │
  │                │                      │                      ├────────────────────────►│ data_offer+offer
  │                │                      │                      │                         │
  │                │                      │                      │ 用户 Ctrl-V             │
  │                │                      │                      │◄────────────────────────┤ offer.receive(m,fd)
  │                │                      │ receive_from_host    │ (origin=Host)           │
  │                │                      │◄─────────────────────┤                         │
  │                │ ConvertSelection     │                      │                         │
  │                │ (mime atom)          │                      │                         │
  │                │◄─────────────────────┤                      │                         │
  │SelectionRequest│                      │                      │                         │
  │◄───────────────┤                      │                      │                         │
  │ChangeProperty  │                      │                      │                         │
  │(bytes | INCR)  │                      │                      │                         │
  ├───────────────►│                      │                      │                         │
  │SelectionNotify │                      │                      │                         │
  ├───────────────►├─────────────────────►│ handle_data_reply    │                         │
  │                │                      │ write(fd, bytes)─────┼────────────────────────►│ 读 fd
  │                │                      │                      │                         │
  │                │ (若 type==INCR)      │                      │                         │
  │                │ PropertyNotify       │                      │                         │
  │                │ NEW_VALUE × N + 空块 │                      │                         │
  │                ├─────────────────────►│ handle_incr_chunk    │                         │
  │                │                      │ 累积 → write(fd)─────┼────────────────────────►│
```

### 内部 Wayland 复制 → 外部 X 粘贴

```
内部 wl client   smithay          emskin state       X11ClipboardProxy       外部 X client
     │              │                  │                    │                      │
     │set_selection │                  │                    │                      │
     ├─────────────►│SelectionHandler  │                    │                      │
     │              │::new_selection   │                    │                      │
     │              ├─────────────────►│ set_host_selection │                      │
     │              │                  ├───────────────────►│ SetSelectionOwner    │
     │              │                  │                    │ (CLIPBOARD, self)    │
     │              │                  │                    ├─────────────────────►│ XFixes notify
     │              │                  │                    │                      │
     │              │                  │                    │       ConvertSelection(TARGETS|mime)
     │              │                  │                    │◄─────────────────────┤
     │              │                  │                    │ on_selection_request │
     │              │                  │                    │ pipe(r,w)            │
     │              │                  │ HostSendRequest    │                      │
     │              │                  │◄───────────────────┤                      │
     │              │request_data_     │                    │                      │
     │              │device_client_    │                    │                      │
     │              │selection(m,w)    │                    │                      │
     │send(m,w)     │◄─────────────────┤                    │                      │
     │◄─────────────┤                  │                    │                      │
     │write(w,data) │                  │                    │                      │
     ├─────────────►│ calloop reads r ─┼───────────────────►│ complete_outgoing    │
     │              │                  │                    │ ChangeProperty       │
     │              │                  │                    │ (bytes | INCR header)│
     │              │                  │                    ├─────────────────────►│
     │              │                  │                    │ SelectionNotify      │
     │              │                  │                    ├─────────────────────►│
     │              │                  │                    │ (INCR 后续按         │
     │              │                  │                    │  PropertyNotify      │
     │              │                  │                    │  DELETE 推 chunk)    │
```

---

## 2. data_control（宿主 Wayland，主路径）

**文件**：`crates/emskin/src/clipboard.rs`

**适用**：wlroots 系（sway / Hyprland / cosmic / niri）、KDE Plasma ≥ 6.2（`ext_data_control_v1`），以及任意同时暴露 `ext_data_control_manager_v1` 或 `zwlr_data_control_manager_v1` 的合成器。

### 要点

- **零焦点门槛**：emskin 随时可读写宿主剪切板，不需要窗口获得键盘焦点。
- 协议优先级：`ext_data_control_v1` → `zwlr_data_control_manager_v1`。接口差异由 `DataControlManager / Device / Offer / Source` 4 个 enum 收拢，业务逻辑走 `ClipboardState::on_*` 共享方法。
- **独立的 wayland 连接**（`Connection::connect_to_env`），与 winit 主连接分离——剪切板不依赖 emskin 的渲染循环。
- `suppress_{clipboard,primary}` 是计数器而非布尔：Firefox 会连发两次 `set_selection`（先不带 SAVE_TARGETS，再带），计数器才能正确吞掉两次回显。

### 外部复制 → 内部粘贴

```
host app   host compositor   ClipboardProxy(独立 wl conn)    smithay dd        内部 wl client
  │             │                     │                          │                    │
  │create source│                     │                          │                    │
  │+offer(mime) │                     │                          │                    │
  │+set_selection                     │                          │                    │
  ├────────────►│                     │                          │                    │
  │             │data_offer(new id)   │                          │                    │
  │             ├────────────────────►│ on_data_offer            │                    │
  │             │offer(mime) × N      │                          │                    │
  │             ├────────────────────►│ on_offer_mime            │                    │
  │             │selection(offer)     │                          │                    │
  │             ├────────────────────►│ on_selection             │                    │
  │             │                     │ HostSelectionChanged     │                    │
  │             │                     ├─────────────────────────►│ set_data_device_   │
  │             │                     │                          │ selection(mimes)   │
  │             │                     │                          ├───────────────────►│ data_offer+offer
  │             │                     │                          │                    │ Ctrl-V
  │             │                     │ receive_from_host        │◄───────────────────┤ offer.receive(m,fd)
  │             │                     │◄─────────────────────────┤ (origin=Host)      │
  │             │offer.receive(m,fd)  │                          │                    │
  │             │◄────────────────────┤                          │                    │
  │source.send  │                     │                          │                    │
  │(mime,fd)    │                     │                          │                    │
  │◄────────────┤                     │                          │                    │
  │write(fd)──────────────────────────────────────────────────────────────────────────►│ 读 fd
```

### 内部复制 → 外部粘贴

```
内部 wl client  smithay      emskin state     ClipboardProxy          host compositor    外部 app
    │              │              │                 │                        │                │
    │set_selection │              │                 │                        │                │
    ├─────────────►│new_selection │                 │                        │                │
    │              ├─────────────►│ set_host_       │                        │                │
    │              │              │ selection(mimes)│                        │                │
    │              │              ├────────────────►│ suppress++             │                │
    │              │              │                 │ create_data_source     │                │
    │              │              │                 │ offer(mime)×N          │                │
    │              │              │                 │ device.set_selection   │                │
    │              │              │                 ├───────────────────────►│                │
    │              │              │                 │  selection echo        │                │
    │              │              │                 │◄───────────────────────┤ on_selection   │
    │              │              │                 │  suppress-- 吞掉       │                │
    │              │              │                 │                        │ data_offer     │
    │              │              │                 │                        ├───────────────►│
    │              │              │                 │                        │ selection      │
    │              │              │                 │                        ├───────────────►│
    │              │              │                 │                        │◄───────────────┤ offer.receive
    │              │              │                 │ source.send(mime,fd)   │                │
    │              │              │                 │◄───────────────────────┤ on_source_send │
    │              │              │ HostSendRequest │                        │                │
    │              │◄─────────────┼─────────────────┤                        │                │
    │              │request_data_ │                 │                        │                │
    │              │device_client_│                 │                        │                │
    │send(mime,fd) │selection     │                 │                        │                │
    │◄─────────────┤              │                 │                        │                │
    │write(fd)──────────────────────────────────────────────────────────────────────────────►│
```

---

## 3. wl_data_device（宿主 Wayland，fallback）

**文件**：`crates/emskin/src/clipboard_wl.rs`

**适用**：KDE Plasma < 6.2（没出 data-control）、GNOME mutter（至今未公开 data-control）、任何只暴露 `wl_data_device_manager` 的合成器。

### 与 data_control 的核心差异

| 维度 | data_control | wl_data_device |
|---|---|---|
| 焦点要求 | 无 | **必须 emskin 窗口有键盘焦点** |
| wayland 连接 | `connect_to_env` 新开 | **共享 winit 的 wl_display**（`Backend::from_foreign_display`） |
| set_selection serial | 不需要 | 必须带输入事件 serial |
| 选择事件送达 | 始终 | 仅在 emskin 窗口被聚焦时 |
| primary selection | 支持 | 本实现未做 |

用户的主场景是"正在用 emskin 时 Ctrl-C/V"，此时焦点天然在 emskin，limitation 不明显。

### serial 来源

连接建立时额外 `seat.get_keyboard()`，在 `Dispatch<WlKeyboard>` 里缓存 `Enter/Leave/Key/Modifiers` 携带的 serial 到 `latest_serial`，set_selection 复用。没缓存到 serial 就静默放弃，等下一轮。

### 外部复制 → 内部粘贴

```
host app   host compositor   WlDataDeviceProxy(共享 winit conn)   smithay dd      内部 client
  │             │                      │                               │               │
  │             │ （用户先把焦点给 emskin 窗口）                        │               │
  │             │ keyboard.enter(serial=S)                              │               │
  │             ├─────────────────────►│ latest_serial = S             │               │
  │             │                      │                               │               │
  │set_selection│                      │                               │               │
  ├────────────►│                      │                               │               │
  │             │ data_offer           │                               │               │
  │             ├─────────────────────►│ pending_offers[id]            │               │
  │             │ offer(mime) × N      │                               │               │
  │             ├─────────────────────►│                               │               │
  │             │ selection(offer)     │                               │               │
  │             ├─────────────────────►│ on_selection                  │               │
  │             │                      │ HostSelectionChanged          │               │
  │             │                      ├──────────────────────────────►│ set_data_     │
  │             │                      │                               │ device_       │
  │             │                      │                               │ selection     │
  │             │                      │                               ├──────────────►│ data_offer
  │             │                      │                               │               │ Ctrl-V
  │             │                      │ receive_from_host             │◄──────────────┤ receive
  │             │                      │◄──────────────────────────────┤               │
  │             │ offer.receive(m,fd)  │                               │               │
  │             │◄─────────────────────┤                               │               │
  │source.send  │                      │                               │               │
  │◄────────────┤                      │                               │               │
  │write(fd)────────────────────────────────────────────────────────────────────────────►│
```

### 内部复制 → 外部粘贴

```
内部 client   smithay      emskin state     WlDataDeviceProxy         host compositor    外部 app
    │            │              │                  │                        │                │
    │set_sel     │              │                  │                        │                │
    ├───────────►│new_selection │                  │                        │                │
    │            ├─────────────►│                  │                        │                │
    │            │              │set_host_selection│                        │                │
    │            │              ├─────────────────►│ latest_serial?         │                │
    │            │              │                  │ ├─ None → 静默放弃     │                │
    │            │              │                  │ └─ Some(S):            │                │
    │            │              │                  │    create_data_source  │                │
    │            │              │                  │    offer(mime)×N       │                │
    │            │              │                  │    device.set_selection(src, S)         │
    │            │              │                  ├───────────────────────►│                │
    │            │              │                  │ selection echo         │                │
    │            │              │                  │◄───────────────────────┤ suppress-- 吞  │
    │            │              │                  │                        │ data_offer     │
    │            │              │                  │                        ├───────────────►│
    │            │              │                  │                        │ selection      │
    │            │              │                  │                        ├───────────────►│
    │            │              │                  │                        │◄───────────────┤ offer.receive
    │            │              │                  │ source.send(m,fd)      │                │
    │            │              │                  │◄───────────────────────┤                │
    │            │              │HostSendRequest   │                        │                │
    │            │◄─────────────┼──────────────────┤                        │                │
    │send(m,fd)  │request_data_ │                  │                        │                │
    │◄───────────┤device_client_│                  │                        │                │
    │write(fd)──────selection───────────────────────────────────────────────────────────────►│
```

### gotchas（写代码时掉进去过）

- `dummy_fd` 是给 calloop 注册的占位 fd；**必须保留 pipe 的写端**，否则 fd 翻成 `POLLHUP` 让 calloop 忙轮询。
- 不自己 `prepare_read`：winit 已经 read 过了，只 `dispatch_pending` 把 libwayland 内部队列里的事件走回调。
- Primary 未实现，`set_host_selection(Primary, ...)` 直接 no-op；真要上得接 `zwp_primary_selection_device_manager_v1`。

---

## 4. XWayland 内部桥（内部 X ↔ 内部 Wayland）

**文件**：`crates/emskin/src/handlers/xwayland.rs` + `crates/emskin/src/clipboard_dispatch.rs`

emskin 内部跑 XWayland 给 X11 Emacs / 传统 X 应用使用。smithay 的 `X11Wm` 暴露 `XwmHandler::{new_selection, send_selection, cleared_selection}` 作为内部 X world ↔ 内部 Wayland world 的桥。**不经过宿主**，宿主同步单独走第 1/2/3 节的路径。

### `SelectionOrigin` 三态

记录当前选择来自哪儿，决定 paste 时该问谁：

- `Wayland` — 内部 wayland 客户端持有
- `X11` — 内部 XWayland 下的 X 客户端持有
- `Host` — 来自宿主剪切板（由 `inject_host_selection` 打标）

### 内部 X 复制 → 内部 wayland 粘贴

```
X client(内部) XWayland        emskin(XwmHandler)       smithay dd         wl client(内部)
  │               │                     │                      │                    │
  │SetSelection   │                     │                      │                    │
  │Owner(CLIP)    │                     │                      │                    │
  ├──────────────►│                     │                      │                    │
  │               │ new_selection       │                      │                    │
  │               ├────────────────────►│                      │                    │
  │               │ (CLIP, mimes)       │ origin = X11         │                    │
  │               │                     │ set_data_device_     │                    │
  │               │                     │ selection            │                    │
  │               │                     ├─────────────────────►│ data_offer         │
  │               │                     │                      ├───────────────────►│
  │               │                     │ (若 IPC 就绪)        │                    │
  │               │                     │ clipboard.set_host_  │                    │
  │               │                     │ selection → 宿主     │                    │
  │               │                     │                      │                    │
  │               │                     │                      │◄───────────────────┤ receive(m,fd)
  │               │                     │ send_selection(m,fd) │                    │
  │               │                     │◄─────────────────────┤                    │
  │               │                     │ origin==X11 →        │                    │
  │               │ xwm.send_selection  │                      │                    │
  │               │◄────────────────────┤                      │                    │
  │SelectionRequest                     │                      │                    │
  │◄──────────────┤                     │                      │                    │
  │ChangeProperty │                     │                      │                    │
  │+SelectionNotify                     │                      │                    │
  ├──────────────►│ 读 property →       │                      │                    │
  │               │ write(fd, bytes)─────────────────────────────────────────────────►│ 读 fd
```

### 内部 wayland 复制 → 内部 X 粘贴

```
wl client     smithay         emskin(XwmHandler)        X11Wm            X client
   │             │                    │                    │                 │
   │set_selection│                    │                    │                 │
   ├────────────►│new_selection       │                    │                 │
   │             ├───────────────────►│ origin = Wayland   │                 │
   │             │                    │ xwm.new_selection  │                 │
   │             │                    ├───────────────────►│ 在代理窗口上     │
   │             │                    │                    │ SetSelectionOwner│
   │             │                    │                    ├────────────────►│ XFixes
   │             │                    │                    │                 │
   │             │                    │                    │ ConvertSelection│
   │             │                    │                    │◄────────────────┤
   │             │                    │ send_selection     │                 │
   │             │                    │ (XwmHandler)       │                 │
   │             │                    │◄───────────────────┤                 │
   │             │                    │ origin==Wayland:   │                 │
   │             │                    │ request_data_      │                 │
   │             │                    │ device_client_     │                 │
   │             │                    │ selection(m,fd)    │                 │
   │             │send(m,fd)          │                    │                 │
   │◄────────────┤                    │                    │                 │
   │write(fd)───►│ X11Wm 读 pipe →    │                    │                 │
   │             │ ChangeProperty ────┼───────────────────►│ SelectionNotify │
   │             │                    │                    ├────────────────►│ 读数据
```

### 宿主 → 内部 X 粘贴（Host origin 回流）

宿主变更先从第 1/2/3 节进 `clipboard_dispatch::inject_host_selection`：

1. 喂 smithay dd（`set_data_device_selection`）给内部 wayland 客户端
2. 喂 `X11Wm::new_selection`（在内部 XWayland root 上声明选择）给内部 X 客户端
3. `origin = Host`，mimes 缓存到 `host_clipboard_mimes`（XWM 晚启动时可 replay）

内部 X 客户端粘贴时：

```
X client → X server → X11Wm → XwmHandler::send_selection(target, mime, fd)
                                          │
                                          │ origin == Host
                                          ▼
                           clipboard.receive_from_host(target, mime, fd)
                                          │
                                          └─▶ 回到第 1/2/3 节的"外部→内部粘贴"下半段
```

### 注意

- `new_selection` 里对 host 的二次推送必须 `ipc.is_connected()` 门控——否则 GTK 启动时的"空选择"会把真实的宿主剪切板清空。
- emskin 对 smithay 打了 patch（`selection/seat_data.rs`）修 GTK3 在焦点切换时丢选择，升级 fork 时别丢。

---

## 参考协议 / 代码位置

### Wayland 协议

- **wayland.xml**（核心协议，`wl_data_device` / `wl_data_source` / `wl_data_offer`）
  <https://gitlab.freedesktop.org/wayland/wayland/-/blob/main/protocol/wayland.xml>
- **ext-data-control-v1**（staging，无焦点剪切板，upstream 首选）
  <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/tree/main/staging/ext-data-control>
- **wlr-data-control-unstable-v1**（wlroots 自家，兼容面最广）
  <https://gitlab.freedesktop.org/wlroots/wlr-protocols/-/blob/master/unstable/wlr-data-control-unstable-v1.xml>
- **primary-selection-unstable-v1**（中键粘贴，wl_data_device 路径本实现未用）
  <https://gitlab.freedesktop.org/wayland/wayland-protocols/-/blob/main/unstable/primary-selection/primary-selection-unstable-v1.xml>

### X11 协议

- **ICCCM §2 "Peer-to-Peer Communication by Means of Selections"**（`SetSelectionOwner` / `ConvertSelection` / `SelectionRequest` / `SelectionNotify` / INCR）
  <https://www.x.org/releases/X11R7.7/doc/xorg-docs/icccm/icccm.html#Peer_to_Peer_Communication_by_Means_of_Selections>
- **XFixes Selection Tracking**（`XFixesSelectSelectionInput`，owner 变化通知，避免轮询）
  <https://www.x.org/releases/X11R7.7/doc/fixesproto/fixesproto.txt>
- **x11rb `SelectionRequestEvent` 文档**
  <https://docs.rs/x11rb/latest/x11rb/protocol/xproto/struct.SelectionRequestEvent.html>

### smithay API

- `smithay::wayland::selection`（`SelectionHandler` / `set_data_device_selection` / `request_data_device_client_selection` / primary 对应项）
  <https://docs.rs/smithay/latest/smithay/wayland/selection/index.html>
- `smithay::xwayland::xwm::XwmHandler`（`new_selection` / `send_selection` / `cleared_selection`）
  <https://docs.rs/smithay/latest/smithay/xwayland/xwm/trait.XwmHandler.html>

### 参考实现

- **anvil**（smithay 官方 demo 的选择桥）：smithay 源码树 `anvil/src/handlers/` 及 `anvil/src/state.rs`
- **sway**：`sway/desktop/xwayland.c` 的 XWM 选择转发
- **wl-clipboard**（`wl-copy` / `wl-paste`，wlr-data-control 最小示例）
  <https://github.com/bugaevc/wl-clipboard>

### emskin 内代码定位

| 路径 | 职责 |
|---|---|
| `crates/emskin/src/clipboard.rs` | `ClipboardBackend` trait + `ClipboardProxy`（ext / wlr data-control） |
| `crates/emskin/src/clipboard_wl.rs` | `WlDataDeviceProxy`（wl_data_device fallback） |
| `crates/emskin/src/clipboard_x11.rs` | `X11ClipboardProxy`（X11 宿主） |
| `crates/emskin/src/clipboard_dispatch.rs` | `HostSelectionChanged` / `HostSendRequest` / `SourceCancelled` 处理中枢 |
| `crates/emskin/src/handlers/xwayland.rs` | `XwmHandler` 的选择三钩子 |
| `crates/emskin/src/state.rs` | `SelectionState` / `SelectionOrigin` 定义 |
| `crates/emskin/tests/e2e_clipboard_wayland.rs` | Wayland 宿主端到端测试（13 条） |
| `crates/emskin/tests/e2e_clipboard_x11.rs` | X11 宿主端到端测试（5 条） |
