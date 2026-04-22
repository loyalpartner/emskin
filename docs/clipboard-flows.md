# emskin 剪切板流程

emskin 是嵌套 Wayland 合成器，需要在 **宿主**（X11 或 Wayland 桌面）与 **内部客户端**（Emacs、EAF、嵌入的 app）之间双向桥接剪切板。

**emskin 内部没有 X 世界**：X 应用（gtk3 Emacs、WeChat、WPS 等）通过外部 `xwayland-satellite` 进程连到 emskin，satellite 把 X 协议翻译成 Wayland 再送进来，所以从 emskin 视角 **所有内部客户端都是 Wayland 客户端（C-W）**。X ↔ Wayland 的协议翻译是 satellite 的责任，不在本文档范围内。

## 组合矩阵

纵轴 = 宿主；横轴 = emskin 内部客户端。

| 宿主 ＼ 内部 | 内部 Wayland 客户端 (C-W) |
|---|---|
| **X11 (H-X11)** | §1 |
| **Wayland, data_control (H-W DC)** | §2 |
| **Wayland, wl_data_device (H-W WD)** | §3 |

关键设计：**emskin 内部只有一条 smithay dd 总线**。所有 world 都喂它，所有 world 都从它取。宿主侧是 smithay dd 的"接线盒"。emskin 既是宿主的 client（向外同步用 `ClipboardProxy` / `WlDataDeviceProxy` / `X11ClipboardProxy`，启动时走 `xdg_activation_v1` 拿焦点），也是内部 compositor（对内部客户端暴露 `ext/wlr data-control` + `wl_data_device`）：

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
       │                            │          (pgtk Emacs、Firefox、         │
                                    │           Electron、xwayland-satellite) │
                                    └─────────────────────────────────────────┘

     ─────── emskin 外侧 ───────
     xwayland-satellite (独立进程)
        ▲
        │ 被 emskin 的 XwlsIntegration 按需拉起
        │ 暴露一个 X DISPLAY（:N），X 客户端连它
        │ 作为普通 wayland client 连回 emskin（上面那条"内部 C-W"线）
```

`SelectionOrigin ∈ {Wayland, Host}` 记录总线里当前选择来自哪个 world，send_selection 才知道 paste fd 该转给谁：
- `Wayland` → `request_data_device_client_selection`（X 客户端通过 satellite 也落到这一支，因为对 emskin 来说它们是普通 wayland 客户端）
- `Host` → `ClipboardBackend::receive_from_host` → 宿主段再 ConvertSelection / offer.receive

**emskin 两侧协议实现要点**：

- **作为宿主的 client**：绑定宿主的 `ext_data_control_v1` / `zwlr_data_control_v1` / `wl_data_device` 三选一做同步（`emskin-clipboard` crate 的 `data_control.rs` / `wl_data_device.rs` / `x11.rs`），并在启动时读 `XDG_ACTIVATION_TOKEN` env、用 `xdg_activation_v1.activate` 把 winit 主窗口激活到聚焦状态（`main.rs::activate_main_surface_if_env_token`）— 这是生产环境 GNOME/KWin startup-notification 唯一合法的 steal-focus 路径，winit 本身不做
- **作为嵌套 compositor server**：把 `wl_data_device_manager` / `wlr_data_control_v1` / `ext_data_control_v1` 都暴露给内部客户端（Firefox、Electron、wl-clipboard、xwayland-satellite 等）— 内部客户端会 prefer DC，所以内部剪切板交互**不吃焦点约束**，和真实 wlroots / KDE ≥ 6.2 桌面一致。内部 `xdg_activation_v1` server 暂未实现（暂无真实需求，内部焦点由 emskin 自己的 auto-focus + Emacs IPC 驱动）

### 时序图约定（读图前请看这条）

1. **emskin 启动时的 `xdg_activation_v1.activate(token, winit_surface)` 步骤在所有 H-W 场景图里省略**（§2 / §3）。它是一次性动作：emskin 从 env 继承 token（由 shell / DBus activation / systemd 传入，测试里由 emez 预生成并 harness 注入），winit 主窗口在此后持续持有宿主焦点。WD 场景下是"emskin 能收到宿主 selection 事件"的前提；DC 场景下不影响协议正确性，但影响生产环境 UX。
2. **所有图里的"内部 wl client"节点**默认通过 `ext_data_control_v1` / `zwlr_data_control_v1` 和 smithay dd 对话（因为 emskin 给内部客户端同时暴露了 DC 和 wl_data_device，客户端优先选 DC）。只画了 `wl_data_device` 路径的老图保留——`set_data_device_selection` 会同时广播给所有绑定的 device 类型，所以两条路径结果一致，差异只在内部客户端**不再**吃 emskin 内部焦点门控。
3. **"X 客户端"在图里不出现**：X 客户端 → xwayland-satellite 进程 → emskin（wayland client）。在 emskin 视角它是一个普通 "内部 wl client"，走 §1/§2/§3 的相应路径。X ↔ Wayland 的转换属于 satellite 内部实现，不在本文档范围内。

## 目录

1. [X11 剪切板（宿主是 X11）](#1-x11-剪切板宿主是-x11)
2. [data_control（宿主 Wayland，主路径）](#2-data_control宿主-wayland主路径)
3. [wl_data_device（宿主 Wayland，fallback）](#3-wl_data_device宿主-waylandfallback)
4. [xwayland-satellite（内部 X 应用）](#4-xwayland-satellite内部-x-应用)
5. [参考协议 / 代码位置](#参考协议--代码位置)

## 角色约定

- **宿主 compositor / X server**：emskin 外部的桌面环境
- **emskin**：合成器本体，同时是宿主的"客户端"
- **smithay dd**：emskin 内部的 `smithay::wayland::selection::data_device` 总线
- **内部客户端 (C-W)**：跑在 emskin 里的 Wayland 应用。X 应用通过 satellite 也映射到这里。

---

## 1. X11 剪切板（宿主是 X11）

**文件**：`crates/emskin-clipboard/src/x11.rs`

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

**文件**：`crates/emskin-clipboard/src/data_control.rs`

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

**文件**：`crates/emskin-clipboard/src/wl_data_device.rs`

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

## 4. xwayland-satellite（内部 X 应用）

**文件**：`crates/emskin/src/xwayland_satellite/`（supervisor）+ 外部 `xwayland-satellite` 进程

emskin 自己 **不运行 X server、不实现 XwmHandler**。内部 X 应用（gtk3 Emacs、WeChat、WPS、IDEA 等）统一通过一个独立的 `xwayland-satellite` 进程接入：

```
┌─ emskin ────────────────────────────────────────────────────────────────┐
│  启动时：                                                                │
│    1. xwayland_satellite::setup_connection(:N)                          │
│       — 绑 /tmp/.X11-unix/X<N> + Linux abstract socket                  │
│    2. XwlsIntegration::arm() 把两个 fd 注册为 calloop Generic 源        │
│       （状态 = Watching）                                                │
│    3. 发 XWaylandReady IPC 给 Emacs（"DISPLAY 已可用"）                 │
│                                                                          │
│  第一个 X client connect :N 触发 Generic 源 → on_socket_connect：       │
│    1. 移除两个 Generic 源                                                │
│    2. 起 spawner 线程 exec                                               │
│         xwayland-satellite :N -listenfd <unix_fd> -listenfd <abs_fd>    │
│       （fd 通过 pre_exec 清 CLOEXEC 继承进子进程）                       │
│    3. 状态 = Running                                                     │
│                                                                          │
│  spawner 线程 child.wait() 阻塞 → child 退出 → 发 ToMain::Rearm         │
│    → main loop 的 channel 回调 drain pending connections、重装          │
│    Generic 源（状态 = Watching）                                         │
└──────────────────────────────────────────────────────────────────────────┘

┌─ xwayland-satellite 进程 ────────────────────────────────────────────────┐
│  作为 wayland client 连 emskin 的 socket                                 │
│  内部起 Xwayland，-listenfd 绑到从 emskin 继承的 X socket                │
│  X 客户端 ↔ Xwayland ↔ satellite ↔ emskin（Wayland）                    │
└──────────────────────────────────────────────────────────────────────────┘
```

从 **剪切板**视角：

- X 客户端在 X 侧 `SetSelectionOwner` → satellite 看到 XFixes 事件 → satellite 作为 wayland client 调 `wl_data_device.set_selection` → 对 emskin 来说就是一个"内部 wl client"发起的 set_selection，走 §1/§2/§3 相应宿主路径，**完全不需要 emskin 知道它是 X 客户端**。
- X 客户端想 paste → Xwayland 通过 satellite 向 emskin 的 data_device 请求 → emskin 按 `SelectionOrigin` 路由给 `Wayland` source（来自内部 wl client）或 `Host` source（来自宿主）。

**emskin 内部 SelectionOrigin 只有两态**：

- `Wayland` — 任意内部 wl client 持有，包括 satellite 代理的 X 客户端
- `Host` — 来自宿主剪切板（由 `inject_host_selection` 打标）

`send_selection` 分发：

| origin | Wayland 粘贴者 (`SelectionHandler::send_selection`) |
|---|---|
| `Wayland` | `request_data_device_client_selection` |
| `Host` | `ClipboardBackend::receive_from_host` |

（老文档里的第三列 `X11 → xwm.send_selection` 已随 `X11Wm` / `handlers/xwayland.rs` 一起删除。）

### satellite 进程生命周期要点

- **按需启动**：emskin 启动时 satellite 进程不跑；第一个 X 客户端连 `:N` 才触发 spawn。Wayland-only 用户（pgtk Emacs + 只用 Wayland 应用）永远不付 satellite 进程开销。
- **崩溃自愈**：satellite 崩溃 → spawner 线程 `wait()` 返回 → `ToMain::Rearm` channel → 主线程 `XwlsIntegration::on_rearm()` 把 `Unlink` 清掉的 socket 重新绑定、重装 Generic 源。下次 X 客户端 connect 会再拉起一次 satellite。
- **不可用时无缝降级**：若 `xwayland-satellite` 二进制不在 `$PATH`（或 `--xwayland-satellite-bin` 指向的路径），`test_ondemand` 返回 false，`start_xwayland_satellite` 打 warn 后直接 return，emskin 以"纯 Wayland"模式继续跑 — X 应用无法嵌入，但 Wayland 应用不受影响。
- **版本依赖**：需要 `xwayland-satellite ≥ 0.7`（`--test-listenfd-support` 探测 + on-demand listenfd 启用协议在这个版本引入）。

### satellite 自身的 X ↔ Wayland 翻译细节

属于 satellite 项目范围，不在本文档。参考上游 [`Supreeeme/xwayland-satellite`](https://github.com/Supreeeme/xwayland-satellite) 的文档和源码，重点文件 `src/xstate.rs` / `src/server/selection.rs`。

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

### 参考实现

- **niri**（niri-style 按需 spawn 模式的来源）：`src/utils/xwayland/` — emskin 的 `XwlsIntegration` 直接 port 自它（GPL-3.0）
- **xwayland-satellite**（上游项目，X ↔ Wayland 协议翻译）
  <https://github.com/Supreeeme/xwayland-satellite>
- **wl-clipboard**（`wl-copy` / `wl-paste`，wlr-data-control 最小示例）
  <https://github.com/bugaevc/wl-clipboard>

### emskin 内代码定位

| 路径 | 职责 |
|---|---|
| `crates/emskin-clipboard/src/data_control.rs` | `ClipboardBackend` trait + `ClipboardProxy`（ext / wlr data-control） |
| `crates/emskin-clipboard/src/wl_data_device.rs` | `WlDataDeviceProxy`（wl_data_device fallback） |
| `crates/emskin-clipboard/src/x11.rs` | `X11ClipboardProxy`（X11 宿主） |
| `crates/emskin/src/clipboard_bridge.rs` | `HostSelectionChanged` / `HostSendRequest` / `SourceCancelled` 处理中枢 |
| `crates/emskin/src/handlers/selection.rs` | `SelectionHandler::new_selection` / `send_selection` |
| `crates/emskin/src/state/mod.rs` | `SelectionState` / `SelectionOrigin`（`Wayland` / `Host`） |
| `crates/emskin/src/xwayland_satellite/sockets.rs` | `X11Sockets` + `setup_connection`（X socket 预绑定） |
| `crates/emskin/src/xwayland_satellite/spawn.rs` | `test_ondemand` 探测 + `build_spawn_command_raw`（`-listenfd` 移交） |
| `crates/emskin/src/xwayland_satellite/watch.rs` | `XwlsIntegration` 状态机 + spawner 线程 + `ToMain::Rearm` |
| `crates/emskin/tests/e2e_clipboard_wayland.rs` | Wayland 宿主端到端测试（5 条：iw↔iw / iw↔ow / iw↔ox / ow→iw / ox→iw） |
| `crates/emskin/tests/e2e_clipboard_wayland_no_data_control.rs` | WD fallback 端到端测试（2 条） |
| `crates/emskin/tests/e2e_clipboard_x11.rs` | X11 宿主端到端测试（3 条） |
