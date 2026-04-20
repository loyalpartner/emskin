# emskin 剪切板流程

emskin 是嵌套 Wayland 合成器，需要在 **宿主**（X11 或 Wayland 桌面）与 **内部客户端**（Emacs、EAF、嵌入的 X11/Wayland app）之间双向桥接剪切板。依据宿主能力与 emskin 内部的 world（Wayland / X11），需要的协议路径共四条，但"端到端"的使用场景是 **宿主 × 内部客户端** 的组合矩阵。

## 组合矩阵

纵轴 = 宿主；横轴 = emskin 内部客户端；cell = 该场景走哪几段路径拼起来。

| 宿主 ＼ 内部 | 内部 Wayland 客户端 (C-W) | 内部 XWayland X 客户端 (C-X) | 内部 C-W ↔ 内部 C-X |
|---|---|---|---|
| **X11 (H-X11)** | §1 单段 | §1 宿主段 + §4 X 桥（两段拼接，见 §5） | §4 |
| **Wayland, data_control (H-W DC)** | §2 单段 | §2 宿主段 + §4 X 桥（两段拼接，见 §5） | §4 |
| **Wayland, wl_data_device (H-W WD)** | §3 单段 | §3 宿主段 + §4 X 桥（两段拼接，见 §5） | §4 |
| **内部 C-X ↔ 内部 C-X** | — | — | X server 自处理，见 §5.3 |

关键设计：**emskin 内部只有一条 smithay dd 总线**。所有 world 都喂它，所有 world 都从它取。宿主侧和内部 X world 都是 smithay dd 的"接线盒"。emskin 既是宿主的 client（向外同步用 `ClipboardProxy` / `WlDataDeviceProxy` / `X11ClipboardProxy`，启动时走 `xdg_activation_v1` 拿焦点），也是内部 compositor（对内部客户端暴露 `ext/wlr data-control` + `wl_data_device`）：

```
     外部宿主 world                 ┌──────── emskin 内部 compositor ────────┐
    (X11 / Wayland)                 │                                         │
     │                              │  暴露给内部客户端的剪切板 globals:      │
     │   ┌─ ClipboardProxy (DC)     │     ext/wlr data_control + wl_data_device │
     │   │  WlDataDeviceProxy (WD)  │                                         │
     │   │  X11ClipboardProxy (X11) │     smithay dd (唯一总线)                │
     ▼   ▼                          │     ▲              ▲                    │
 ┌───────────────────┐  §1/2/3      │     │              │                    │
 │ ClipboardBackend  │◄────────────►│─────┘              │                    │
 │ set/receive_from  │              │                    │ smithay            │
 └───────────────────┘              │                    │ SelectionHandler   │
       ▲                            │                    │                    │
       │ xdg_activation_v1          │            ┌───────┴──────┐             │
       │ (启动时从 env 读 token     │            │ 内部 C-W     │             │
       │  激活 winit 主窗口)        │            └──────────────┘             │
       │                            │                                         │
                                    │            ┌──────────────┐             │
                                    │            │ 内部 C-X     │◄──────────► X11Wm / XwmHandler  §4
                                    │            └──────────────┘             │
                                    └─────────────────────────────────────────┘
```

`SelectionOrigin ∈ {Wayland, X11, Host}` 记录总线里当前选择来自哪个 world，send_selection 才知道 paste fd 该转给谁：
- `Wayland` → `request_data_device_client_selection`
- `X11` → `xwm.send_selection`
- `Host` → `ClipboardProxy::receive_from_host` → 宿主段再 ConvertSelection / offer.receive

**emskin 两侧协议实现要点**：

- **作为宿主的 client**：绑定宿主的 `ext_data_control_v1` / `zwlr_data_control_v1` / `wl_data_device` 三选一做同步（`clipboard.rs` / `clipboard_wl.rs` / `clipboard_x11.rs`），并在启动时读 `XDG_ACTIVATION_TOKEN` env、用 `xdg_activation_v1.activate` 把 winit 主窗口激活到聚焦状态（`main.rs::activate_main_surface_if_env_token`）— 这是生产环境 GNOME/KWin startup-notification 唯一合法的 steal-focus 路径，winit 本身不做
- **作为嵌套 compositor server**：把 `wl_data_device_manager` / `wlr_data_control_v1` / `ext_data_control_v1` 都暴露给内部客户端（Firefox、Electron、wl-clipboard 等）— 内部客户端会 prefer DC，所以内部剪切板交互**不吃焦点约束**，和真实 wlroots / KDE ≥ 6.2 桌面一致。内部 `xdg_activation_v1` server 暂未实现（暂无真实需求，内部焦点由 emskin 自己的 auto-focus + Emacs IPC 驱动）

### 时序图约定（读图前请看这条）

1. **emskin 启动时的 `xdg_activation_v1.activate(token, winit_surface)` 步骤在所有 H-W 场景图里省略**（§2 / §3 / §5.3 / §5.4 / §5.6 / §5.7）。它是一次性动作：emskin 从 env 继承 token（由 shell / DBus activation / systemd 传入，测试里由 emez 预生成并 harness 注入），winit 主窗口在此后持续持有宿主焦点。WD 场景下是"emskin 能收到宿主 selection 事件"的前提；DC 场景下不影响协议正确性，但影响生产环境 UX。
2. **所有图里的"内部 wl client"节点**默认通过 `ext_data_control_v1` / `zwlr_data_control_v1` 和 smithay dd 对话（因为 emskin 给内部客户端同时暴露了 DC 和 wl_data_device，客户端优先选 DC）。只画了 `wl_data_device` 路径的老图保留——`set_data_device_selection` 会同时广播给所有绑定的 device 类型，所以两条路径结果一致，差异只在内部客户端**不再**吃 emskin 内部焦点门控。

## 目录

1. [X11 剪切板（宿主是 X11）](#1-x11-剪切板宿主是-x11)
2. [data_control（宿主 Wayland，主路径）](#2-data_control宿主-wayland主路径)
3. [wl_data_device（宿主 Wayland，fallback）](#3-wl_data_device宿主-waylandfallback)
4. [XWayland 内部桥（内部 X ↔ 内部 Wayland）](#4-xwayland-内部桥内部-x--内部-wayland)
5. [宿主 ↔ 内部 X 客户端（跨层组合）](#5-宿主--内部-x-客户端跨层组合)
6. [参考协议 / 代码位置](#参考协议--代码位置)

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

### 焦点获取：startup-notification / xdg_activation_v1

生产环境 emskin 从 shell / DBus 启动时，宿主（GNOME / KWin）会通过 `XDG_ACTIVATION_TOKEN` / `DESKTOP_STARTUP_ID` 环境变量传入一个激活 token。emskin 在 `main.rs::activate_main_surface_if_env_token` 里读这个 token，绑定 `xdg_activation_v1` global，对 winit 主 `wl_surface` 调 `activate(token, surface)` — 让宿主按协议合法地把焦点给 emskin。winit 自己不管 startup-notification，所以这一步由 emskin 手动完成。

如果 token 不存在或宿主不支持 `xdg_activation_v1`，`activate_main_surface_if_env_token` 安静 no-op；此时 emskin 依赖用户手动点击 / alt-tab 获得焦点，这是 Mutter-like 宿主下的现实限制。

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
- 测试用的 emez 宿主里内置了一个"剪切板管理器"（`--no-data-control` 模式下启用）：当外部 wl-copy 设置选择时，emez 把所有 mime 数据读进内存，用 compositor-owned selection 接管，并把焦点还给 emskin 主窗口。没有这个，wl-copy fork daemon 会持续持有宿主焦点，emskin 的 WD proxy 永远收不到 selection 事件（WD 协议的硬性限制）。真实 GNOME / KWin 下对应的生态工具是 `wl-clip-persist`、`clipman` 等第三方剪切板守护进程——同一思想，换位置实现。详见 `crates/emez/CLAUDE.md`。

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

## 5. 宿主 ↔ 内部 X 客户端（跨层组合）

**文件**：`crates/emskin/src/clipboard_dispatch.rs` + `handlers/xwayland.rs`

emskin 里没有"宿主↔内部 X"的专用路径。任何"外部宿主 app 与内部 XWayland 下 X 客户端"的复制/粘贴，都是 **§1/§2/§3 宿主段** 与 **§4 内部 X 桥** 的两段拼接，在 smithay dd 总线与 `SelectionOrigin` 状态机上"对接"。

本节逐条画完整拼接图，并**明确 data_control 与 wl_data_device 在跨层场景下的差异**。

### 5.1 `SelectionOrigin` 状态机（跨层路由的关键）

`send_selection`（不论 `SelectionHandler` 还是 `XwmHandler`）都按 origin 决定 fd 转给谁：

| origin | Wayland 粘贴者 (`SelectionHandler::send_selection`) | X 粘贴者 (`XwmHandler::send_selection`) |
|---|---|---|
| `Wayland` | `request_data_device_client_selection` | `request_data_device_client_selection` |
| `X11` | `xwm.send_selection` | **X server 直达**，emskin 不介入（fd 丢弃） |
| `Host` | `ClipboardProxy::receive_from_host` | `ClipboardProxy::receive_from_host` |

origin 的赋值点：
- 外部宿主变化 → `inject_host_selection`：`origin = Host`（同时喂 smithay dd + `xwm.new_selection`）
- 内部 C-W `set_selection` → smithay `SelectionHandler::new_selection`：`origin = Wayland`
- 内部 C-X `SetSelectionOwner` → `XwmHandler::new_selection`：`origin = X11`（同时喂 smithay dd + `ClipboardProxy::set_host_selection`）
- `cleared_selection` / `SourceCancelled` → `origin = default`（即 `Wayland`）

### 5.2 H-X11 外部 X 复制 → 内部 C-X 粘贴

```
外部 X owner   X server / XFixes    X11ClipboardProxy     inject_host_      smithay dd     X11Wm         内部 X client
                                    (clipboard_x11.rs)    selection         (总线)         (代理窗口)
  │                 │                     │                   │                │             │                 │
  │ SetSelectionOwner (CLIPBOARD)         │                   │                │             │                 │
  ├────────────────►│                     │                   │                │             │                 │
  │                 │ XFixesSelectionNotify                   │                │             │                 │
  │                 ├────────────────────►│ on_xfixes_notify  │                │             │                 │
  │                 │ ConvertSelection(TARGETS)               │                │             │                 │
  │                 │◄────────────────────┤                   │                │             │                 │
  │ SelectionNotify │                     │                   │                │             │                 │
  ├────────────────►├────────────────────►│ atom → mime       │                │             │                 │
  │                 │                     │ HostSelectionChanged                             │                 │
  │                 │                     ├──────────────────►│ set_data_device_selection     │                 │
  │                 │                     │                   ├───────────────►│             │                 │
  │                 │                     │                   │ origin = Host  │             │                 │
  │                 │                     │                   │ xwm.new_selection             │                 │
  │                 │                     │                   ├──────────────────────────────►│ 代理窗口:       │
  │                 │                     │                   │                │             │ SetSelectionOwner│
  │                 │                     │                   │                │             ├────────────────►│ XFixes 通知
  │                 │                     │                   │                │             │                 │
  │                 │                     │                   │                │             │ 用户 Ctrl-V     │
  │                 │                     │                   │                │             │◄────────────────┤ ConvertSelection
  │                 │                     │                   │                │             │ XwmHandler::send_selection
  │                 │                     │                   │ origin==Host   │             │ (target,mime,fd)│
  │                 │                     │◄ receive_from_host┼────────────────┼─────────────┤                 │
  │                 │ ConvertSelection(mime atom)             │                │             │                 │
  │                 │◄────────────────────┤                   │                │             │                 │
  │ SelectionRequest│                     │                   │                │             │                 │
  │◄────────────────┤                     │                   │                │             │                 │
  │ ChangeProperty(bytes | INCR header)   │                   │                │             │                 │
  ├────────────────►│                     │                   │                │             │                 │
  │ SelectionNotify │                     │                   │                │             │                 │
  ├────────────────►├────────────────────►│ handle_data_reply │                │             │                 │
  │                 │                     │ write(fd, bytes) ─┼────────────────┼─────────────┼────────────────►│ X11Wm ChangeProperty
  │                 │                     │                   │                │             │                 │ + SelectionNotify
  │                 │ (INCR: PropertyNotify NEW_VALUE × N → handle_incr_chunk → write(fd))   │                 │ → X client 读取
```

### 5.3 H-W DC 外部 Wayland 复制 → 内部 C-X 粘贴

```
外部 W app   host compositor   ClipboardProxy(独立连接)    inject_host_    smithay dd     X11Wm          内部 X client
                                (clipboard.rs)            selection       (总线)         (代理窗口)
  │              │                    │                        │              │             │                   │
  │ create_data_source + offer(mime)×N + set_selection         │              │             │                   │
  ├─────────────►│                    │                        │              │             │                   │
  │              │ data_offer(new id) │                        │              │             │                   │
  │              ├───────────────────►│ on_data_offer          │              │             │                   │
  │              │ offer(mime) × N    │                        │              │             │                   │
  │              ├───────────────────►│ on_offer_mime          │              │             │                   │
  │              │ selection(offer)   │                        │              │             │                   │
  │              ├───────────────────►│ on_selection           │              │             │                   │
  │              │                    │ HostSelectionChanged   │              │             │                   │
  │              │                    ├───────────────────────►│ set_data_device_selection   │                   │
  │              │                    │                        ├─────────────►│             │                   │
  │              │                    │                        │ origin = Host│             │                   │
  │              │                    │                        │ xwm.new_selection           │                   │
  │              │                    │                        ├─────────────────────────────►│ 代理窗口:         │
  │              │                    │                        │              │             │ SetSelectionOwner  │
  │              │                    │                        │              │             ├──────────────────►│ XFixes 通知
  │              │                    │                        │              │             │                   │
  │              │                    │                        │              │             │ 用户 Ctrl-V       │
  │              │                    │                        │              │             │◄──────────────────┤ ConvertSelection
  │              │                    │                        │              │             │ XwmHandler::send_selection
  │              │                    │                        │ origin==Host │             │ (target,mime,fd)  │
  │              │                    │◄ receive_from_host─────┼──────────────┼─────────────┤                   │
  │              │                    │ offer.receive(mime,fd) │              │             │                   │
  │              │◄───────────────────┤                        │              │             │                   │
  │ source.send(mime, fd)             │                        │              │             │                   │
  │◄─────────────┤                    │                        │              │             │                   │
  │ write(fd, bytes)──────────────────────────────────────────────────────────────────────────────────────────────►│ X11Wm ChangeProperty
  │              │                    │                        │              │             │                   │ + SelectionNotify
  │              │                    │                        │              │             │                   │ → X client 读取
```

**DC 特点**：独立 wayland 连接，**无焦点门槛** — 任何时候都能感知外部变化并把 fd 回流。

### 5.4 H-W WD 外部 Wayland 复制 → 内部 C-X 粘贴

```
外部 W app   host compositor   WlDataDeviceProxy(共享 winit conn)  inject_host_  smithay dd   X11Wm      内部 X client
                                (clipboard_wl.rs)                  selection     (总线)       (代理窗口)
  │              │                          │                           │            │          │                │
  │              │ (用户先把焦点给 emskin 主窗口)                         │            │          │                │
  │              │ keyboard.enter(serial=S) │                           │            │          │                │
  │              ├─────────────────────────►│ latest_serial = S         │            │          │                │
  │              │                          │                           │            │          │                │
  │ set_selection│                          │                           │            │          │                │
  ├─────────────►│                          │                           │            │          │                │
  │              │ data_offer / offer × N / selection(offer)            │            │          │                │
  │              ├─────────────────────────►│ pending_offers / on_selection          │          │                │
  │              │                          │ HostSelectionChanged      │            │          │                │
  │              │                          ├──────────────────────────►│ set_data_device_selection               │
  │              │                          │                           ├───────────►│          │                │
  │              │                          │                           │ origin=Host│          │                │
  │              │                          │                           │ xwm.new_selection                      │
  │              │                          │                           ├──────────────────────►│ 代理窗口:        │
  │              │                          │                           │            │          │ SetSelectionOwner│
  │              │                          │                           │            │          ├───────────────►│ XFixes 通知
  │              │                          │                           │            │          │                │
  │              │                          │                           │            │          │ 用户 Ctrl-V    │
  │              │                          │                           │            │          │◄───────────────┤ ConvertSelection
  │              │                          │                           │            │          │ XwmHandler::send_selection
  │              │                          │                           │ origin=Host│          │ (target,mime,fd)
  │              │                          │◄ receive_from_host────────┼────────────┼──────────┤                │
  │              │ offer.receive(mime,fd)   │                           │            │          │                │
  │              │◄─────────────────────────┤                           │            │          │                │
  │ source.send(mime,fd)                    │                           │            │          │                │
  │◄─────────────┤                          │                           │            │          │                │
  │ write(fd)────────────────────────────────────────────────────────────────────────────────────────────────────►│ X11Wm ChangeProperty
  │              │                          │                           │            │          │                │ + SelectionNotify
  │              │                          │                           │            │          │                │ → X client 读取
```

**WD 特点**：共享 winit 连接。**外部复制发生瞬间 emskin 主窗口必须有键盘焦点**（否则宿主根本不发 `selection(offer)` 给 emskin），一旦收到就与 DC 同路径。

### 5.5 内部 C-X 复制 → H-X11 外部 X 粘贴

```
内部 X client  X11Wm        XwmHandler       smithay dd    X11ClipboardProxy    X server       外部 X client
                            ::new_selection  (总线)         (clipboard_x11.rs)
    │           │                │                │                │                │                │
    │ SetSelectionOwner          │                │                │                │                │
    ├──────────►│                │                │                │                │                │
    │           │ new_selection  │                │                │                │                │
    │           ├───────────────►│ set_data_device_selection       │                │                │
    │           │                ├───────────────►│ C-W 可见       │                │                │
    │           │                │ origin = X11   │                │                │                │
    │           │                │ (ipc.is_connected()==true)      │                │                │
    │           │                │ set_host_selection              │                │                │
    │           │                ├────────────────────────────────►│ suppress_clipboard++            │
    │           │                │                │                │ SetSelectionOwner(CLIPBOARD,proxy)│
    │           │                │                │                ├───────────────►│ XFixes echo    │
    │           │                │                │                │◄───────────────┤ suppress-- 吞  │
    │           │                │                │                │                │                │
    │           │                │                │                │                │ 用户 Ctrl-V    │
    │           │                │                │                │                │◄───────────────┤ ConvertSelection(TARGETS/mime)
    │           │                │                │                │◄───────────────┤                │
    │           │                │                │                │ on_selection_request            │
    │           │                │                │                │ pipe(r,w)                       │
    │           │                │                │                │ HostSendRequest                 │
    │           │                │◄───────────────┼────────────────┤                │                │
    │           │                │ forward_client_selection                         │                │
    │           │                │ origin==X11 → xwm.send_selection(target,mime,fd=w)               │
    │           │ xwm.send_      │                │                │                │                │
    │◄──────────┤ selection      │                │                │                │                │
    │           │                │                │                │                │                │
    │ SelectionRequest           │                │                │                │                │
    │ ChangeProperty             │                │                │                │                │
    ├──────────►│ 读 property →  │                │                │                │                │
    │           │ write(fd=w) ───┼────────────────┼───────────────►│ calloop reads r                 │
    │           │                │                │                │ complete_outgoing               │
    │           │                │                │                │ ChangeProperty(bytes|INCR)      │
    │           │                │                │                ├───────────────►│ SelectionNotify│
    │           │                │                │                │                ├───────────────►│ 读取
    │           │                │                │                │ (INCR: PropertyNotify DELETE → push chunk)   │
```

### 5.6 内部 C-X 复制 → H-W DC 外部 Wayland 粘贴

```
内部 X client  X11Wm   XwmHandler       smithay dd    ClipboardProxy(独立连接)  host compositor   外部 W app
                      ::new_selection   (总线)        (clipboard.rs)
    │           │          │                │               │                        │                  │
    │ SetSelectionOwner    │                │               │                        │                  │
    ├──────────►│          │                │               │                        │                  │
    │           │new_sel.  │                │               │                        │                  │
    │           ├─────────►│set_data_device_selection       │                        │                  │
    │           │          ├───────────────►│ C-W 可见      │                        │                  │
    │           │          │origin=X11      │               │                        │                  │
    │           │          │(ipc.connected) │               │                        │                  │
    │           │          │set_host_selection              │                        │                  │
    │           │          ├───────────────────────────────►│ suppress++             │                  │
    │           │          │                │               │ create_data_source     │                  │
    │           │          │                │               │ offer(mime)×N          │                  │
    │           │          │                │               │ device.set_selection   │                  │
    │           │          │                │               ├───────────────────────►│ selection echo   │
    │           │          │                │               │◄───────────────────────┤ suppress-- 吞    │
    │           │          │                │               │                        │ data_offer       │
    │           │          │                │               │                        ├─────────────────►│
    │           │          │                │               │                        │ selection        │
    │           │          │                │               │                        ├─────────────────►│
    │           │          │                │               │                        │                  │ Ctrl-V
    │           │          │                │               │                        │◄─────────────────┤ offer.receive(mime,fd)
    │           │          │                │               │ source.send(mime,fd)   │                  │
    │           │          │                │               │◄───────────────────────┤ on_source_send   │
    │           │          │                │               │ HostSendRequest        │                  │
    │           │          │◄───────────────┼───────────────┤                        │                  │
    │           │          │ forward_client_selection, origin==X11 → xwm.send_selection                 │
    │           │xwm.send_ │                │               │                        │                  │
    │◄──────────┤selection │                │               │                        │                  │
    │ SelReq / ChangeProperty               │               │                        │                  │
    ├──────────►│ 读 property →             │               │                        │                  │
    │           │ write(fd) ─────────────────────────────────►│ 转发到外部 (fd 即 offer.receive 的 w 端) │
    │           │          │                │               ├───────────────────────────────────────────►│ 读取
```

### 5.7 内部 C-X 复制 → H-W WD 外部 Wayland 粘贴

```
内部 X  X11Wm  XwmHandler     smithay dd   WlDataDeviceProxy(共享 winit conn)  host compositor   外部 W app
client         ::new_sel.     (总线)       (clipboard_wl.rs)
  │       │       │                │               │                              │                │
  │       │       │                │               │ keyboard.enter/key           │                │
  │       │       │                │               │ latest_serial = S            │                │
  │       │       │                │               │◄─────────────────────────────┤                │
  │SetSel │       │                │               │                              │                │
  │Owner  │       │                │               │                              │                │
  ├──────►│new_sel│                │               │                              │                │
  │       ├──────►│set_data_device_selection       │                              │                │
  │       │       ├───────────────►│ C-W 可见      │                              │                │
  │       │       │ origin=X11     │               │                              │                │
  │       │       │ (ipc.connected)│               │                              │                │
  │       │       │ set_host_selection             │                              │                │
  │       │       ├───────────────────────────────►│ latest_serial?               │                │
  │       │       │                │               │ ├─ None → 静默放弃，终止     │                │
  │       │       │                │               │ └─ Some(S):                  │                │
  │       │       │                │               │    create_data_source        │                │
  │       │       │                │               │    offer(mime)×N             │                │
  │       │       │                │               │    device.set_selection(src,S)│                │
  │       │       │                │               ├─────────────────────────────►│ selection echo │
  │       │       │                │               │◄─────────────────────────────┤ suppress-- 吞  │
  │       │       │                │               │                              │ data_offer     │
  │       │       │                │               │                              ├───────────────►│
  │       │       │                │               │                              │ selection      │
  │       │       │                │               │                              ├───────────────►│ Ctrl-V
  │       │       │                │               │                              │◄───────────────┤ offer.receive(m,fd)
  │       │       │                │               │ source.send(mime,fd)         │                │
  │       │       │                │               │◄─────────────────────────────┤                │
  │       │       │                │               │ HostSendRequest              │                │
  │       │       │◄───────────────┼───────────────┤                              │                │
  │       │       │ forward_client_selection, origin==X11 → xwm.send_selection    │                │
  │       │xwm.send│               │               │                              │                │
  │◄──────┤sel    │                │               │                              │                │
  │ SelReq/ChangeProperty          │               │                              │                │
  ├──────►│ write(fd) ──────────────────────────────►│ 写入 offer.receive 的 w 端                   │
  │       │       │                │               ├─────────────────────────────────────────────►│ 读取
```

**WD 独有风险**：若 `latest_serial == None`（emskin 主窗口从未被聚焦过），整条路径在 `set_host_selection` 处就会静默放弃；外部 Wayland 应用不会感知到选择变化。

### 5.8 内部 C-X ↔ 内部 C-X（X server 自处理）

同一 XWayland 下两个 X 客户端互相粘贴时，emskin 完全不介入 — X 协议在 XWayland 进程内部自己处理 `SelectionRequest`。`XwmHandler::new_selection` 仍会被调到（用于把选择桥到 smithay dd 和宿主），但真正的 paste 走 X server 内部广播。

若 `XwmHandler::send_selection` 被错误触发（origin==X11 时 Wayland 侧粘贴者申请，而粘贴者自己是同 XWayland 的 X client），`handlers/xwayland.rs:307` 直接 drop fd，让 X server 自己完成。

### 5.9 完整场景索引（所有复制→粘贴组合）

符号：**H-X11** 宿主 X11；**H-W DC** 宿主 Wayland data_control；**H-W WD** 宿主 Wayland wl_data_device；**C-W** 内部 Wayland 客户端；**C-X** 内部 XWayland 下的 X 客户端。

| # | 复制方 | 粘贴方 | 宿主模式 | 看图去这里 | origin | 宿主焦点要求 | 内部路径 |
|---|---|---|---|---|---|---|---|
| 1 | H-X11 外部 X | C-W | X11 | §1「外部 X 复制 → 内部 Wayland 粘贴」 | `Host` | 无 | DC（无焦点）|
| 2 | C-W | H-X11 外部 X | X11 | §1「内部 Wayland 复制 → 外部 X 粘贴」 | `Wayland` | 无 | DC |
| 3 | H-X11 外部 X | C-X | X11 | **§5.2** | `Host` | 无 | XWM 桥 |
| 4 | C-X | H-X11 外部 X | X11 | **§5.5** | `X11` | 无 | XWM 桥 |
| 5 | H-X11 外部 X ↔ H-X11 外部 X | — | X11 | X server 自处理，emskin 不参与 | n/a | — | — |
| 6 | H-W DC 外部 | C-W | W+DC | §2「外部复制 → 内部粘贴」 | `Host` | 无 | DC（无焦点）|
| 7 | C-W | H-W DC 外部 | W+DC | §2「内部复制 → 外部粘贴」 | `Wayland` | 无 | DC |
| 8 | H-W DC 外部 | C-X | W+DC | **§5.3** | `Host` | 无 | XWM 桥 |
| 9 | C-X | H-W DC 外部 | W+DC | **§5.6** | `X11` | 无 | XWM 桥 |
| 10 | H-W DC 外部 ↔ H-W DC 外部 | — | W+DC | 宿主处理，emskin 不参与 | n/a | — | — |
| 11 | H-W WD 外部 | C-W | W+WD | §3「外部复制 → 内部粘贴」 | `Host` | **emskin 主窗口需持焦点**（启动时 xdg_activation 获取）| DC（内部无焦点约束）|
| 12 | C-W | H-W WD 外部 | W+WD | §3「内部复制 → 外部粘贴」 | `Wayland` | **同 #11 + 需要 latest_serial** | DC |
| 13 | H-W WD 外部 | C-X | W+WD | **§5.4** | `Host` | **同 #11** | XWM 桥 |
| 14 | C-X | H-W WD 外部 | W+WD | **§5.7** | `X11` | **同 #12** | XWM 桥 |
| 15 | H-W WD 外部 ↔ H-W WD 外部 | — | W+WD | 宿主处理，emskin 不参与 | n/a | — | — |
| 16 | C-W | C-W | 无关 | §4 前提（smithay dd 自环）| `Wayland` | 无 | DC（无焦点）|
| 17 | C-X | C-X | 无关 | **§5.8**（X server 自环）| `X11` | 无 | X server |
| 18 | C-W | C-X | 无关 | §4「内部 wayland 复制 → 内部 X 粘贴」 | `Wayland` | 无 | DC + XWM 桥 |
| 19 | C-X | C-W | 无关 | §4「内部 X 复制 → 内部 wayland 粘贴」 | `X11` | 无 | XWM 桥 + DC |

**关键观察**：

- **"宿主焦点要求"只影响 WD 宿主段（#11~#14）**。在 H-W WD 下，emskin 必须能从宿主收到 `selection(offer)` 事件，这要求 winit 主窗口持有宿主焦点——通过启动时的 `xdg_activation_v1.activate` 自动获取，而不是依赖用户手动聚焦 emskin。
- **"内部路径"对所有宿主模式一致**：内部客户端（C-W / C-X）现在都通过 emskin 暴露的 `ext/wlr data_control` 或 XWM 桥跟 smithay dd 对话，不吃 emskin 内部焦点门控。这正是给内部暴露 DC 的生产收益。
- **跨层宿主→C-X（#3/#8/#13）都是"宿主段 + §4 X 桥"两段拼接**：粘合剂是 smithay dd 总线 + `SelectionOrigin=Host` + `host_clipboard_mimes` 缓存。
- **跨层 C-X→宿主（#4/#9/#14）的核心是 `XwmHandler::new_selection` 里 `set_host_selection`**：同时把选择发布到内部 smithay dd 和外部宿主；粘贴回流靠 `forward_client_selection(origin==X11)` 路由。
- **纯内部场景（#16~#19）与宿主完全解耦**，即使 emskin 失去宿主焦点或宿主崩溃，内部客户端之间依然能正常同步（内部 smithay dd 总线仍然工作）。

### 5.10 跨层易错点

- **`XwmHandler::new_selection` 里的 `set_host_selection` 必须 `ipc.is_connected()` 门控**：GTK Emacs 启动时会发一次空选择，不门控会把真实宿主剪切板清空。
- **`SourceCancelled` / `cleared_selection` 必须把 origin 复位为 `Wayland`**（`SelectionOrigin::default()`）：否则下一次 `send_selection` 会按旧 origin 去问不存在的 source。
- **WD 场景依赖 emskin 主窗口启动时通过 `xdg_activation_v1.activate` 拉到焦点**。如果 `XDG_ACTIVATION_TOKEN` env 为空且宿主不暴露 `xdg_activation_v1` global，`activate_main_surface_if_env_token` 安静 no-op，此时 emskin 要靠用户手动点击才能获得焦点——WD 路径功能上仍然工作，但"无缝启动即可用"的体验会退化。运行时只会选其中一条宿主段，不会"降级走 DC"。
- `host_clipboard_mimes` / `host_primary_mimes` 缓存：XWM 晚于首次 `HostSelectionChanged` 就绪时，`inject_host_selection` 的 `xwm.new_selection` 会被跳过，靠 XWM 就绪回调从 cache replay。
- **内部 C-W 绑定 DC 还是 wl_data_device 不影响 selection 正确性**：smithay 的 `set_data_device_selection` 会同时广播给所有绑定的 device 类型；但**吃不吃 emskin 内部焦点门控**完全取决于客户端选的是 DC（不吃）还是 wl_data_device（吃）。wl-clipboard / Firefox / Electron 都 prefer DC，所以生产场景里内部焦点问题基本绝迹。


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
