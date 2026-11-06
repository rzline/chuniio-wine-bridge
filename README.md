# chuniio-wine-bridge

[**English**](README.en.md) | 简体中文

面向 **Linux + Wine** 环境的 RPunithm 手台桥接工具，通过文件共享内存在 Linux 原生 HID 访问与 Wine 侧 `chuniio` 接口之间传递输入和灯光数据。

项目由 Rust 命令行程序 `chuni-bridge-probe`、Windows `chuniio.dll` 及配套诊断工具组成。Linux 负责访问设备，DLL 只读写共享内存，不直接操作 USB/HID。

## 目录

- [功能与架构](#功能与架构)
- [环境要求](#环境要求)
- [快速开始](#快速开始)
- [Wine 侧 DLL 构建与配置](#wine-侧-dll-构建与配置)
- [命令参考](#命令参考)
- [LED 调试](#led-调试)
- [共享内存与故障保护](#共享内存与故障保护)
- [测试与验证](#测试与验证)
- [常见问题](#常见问题)
- [项目结构](#项目结构)
- [贡献与授权](#贡献与授权)

## 功能与架构

- **真实 HID 输入**：默认匹配 RPunithm `VID:PID = 1973:2001`、接口 `2`，严格解析 45 字节输入报告，提取 32 路压力、空气感应和按钮数据。
- **跨进程输入传递**：Linux 发布原始通道顺序的输入快照；Wine DLL 在游戏接口边界转换通道顺序。
- **滑条灯光回传**：DLL 发布游戏 LED 数据，Linux `bridge` 将其组装成 HID 输出报告，驱动已验证设备上的 31 颗滑条灯。
- **失效保护与重发现**：设备读取异常、报告超时或生产者失活时将硬件输入归零，并在 Linux 侧轮询重新发现设备。
- **诊断工具**：提供设备枚举、报告采集、合成输入、LED 校准、双位数 Wine 验证及软件链路延迟观测工具。

共享文件默认位于 Linux 的 `/dev/shm/chuni_bridge_probe`，Wine 侧默认通过 `Z:\dev\shm\chuni_bridge_probe` 访问。两端映射的是**同一 backing file**，不是两套互不相通的命名共享内存。

### 支持范围

| 项目 | 当前范围 |
| --- | --- |
| 原生桥接端 | x86 / x86_64 Linux，HIDAPI hidraw 后端 |
| Wine 接口端 | x86 / x64 Windows PE DLL，chuniio API `0x0102` |
| 目标设备 | RPunithm `1973:2001`，桥接固定使用接口 2 |
| 已有运行器验证 | Wine 10.20；其他 Wine / Proton 版本需单独验证 |
| 灯光 | 32 个接口区号映射到 31 颗物理滑条灯；区号 31 无物理灯 |
| 空气灯与读卡 | 已验证的 ToF 手台无空气塔灯；兼容颜色字段保留在共享内存中，不输出到设备；读卡未实现 |

## 环境要求

### Linux 命令行程序

- Rust **1.89 或更新版本**，以及 Cargo。
- C 编译工具链和 `pkg-config`。
- 提供 `hidapi-hidraw.pc` 的系统 HIDAPI 开发文件。
- 当前用户对目标接口的 hidraw 节点具有读写权限。

检查依赖：

```sh
rustc --version
cargo --version
pkg-config --modversion hidapi-hidraw
```

### 可选开发工具

- **DLL / PE 探针构建**：Clang、LLD（`lld-link`）、`llvm-nm`、`llvm-dlltool`、Python 3，以及包含 Windows 开发头文件的 Wine 目录。
- **Wine 验证**：支持 32 位和 64 位 PE 的 Wine 运行器；映射探针的构建还需要 Wine 对应架构的导入库。
- **Python 诊断工具**：Python 3；DLL 构建脚本使用 `str.removeprefix`，需 Python 3.9+。延迟观测工具另需 `libatomic.so.1`。

## 快速开始

以下命令均在项目根目录执行。**不要以 root 身份运行桥接程序或游戏。**

### 1. 构建程序

> 也可以直接从 [GitHub Releases](../../releases) 下载预编译的 `chuni-bridge-probe`、32/64 位 `chuniio.dll` 与 udev 规则包。

```sh
cargo build --locked --release
./target/release/chuni-bridge-probe --help
```

### 2. 检查设备

连接手台后执行：

```sh
./target/release/chuni-bridge-probe hid-list
./target/release/chuni-bridge-probe hid-read --reports 20 --duration-secs 10
```

`hid-list --all` 可显示所有 HID 枚举项。程序选择设备时按路径去重；存在多台匹配设备时不会猜选，可使用本次枚举得到的路径：

```sh
./target/release/chuni-bridge-probe hid-read --device-path /dev/hidrawN
```

这里的 `/dev/hidrawN` 是占位符，必须替换成实际节点。重新插拔后节点编号可能变化，不应将其作为固定配置。

### 3. 配置设备权限

仓库提供仅匹配目标设备接口 2 的 udev 规则，需手动安装：

```sh
sudo install -Dm644 packaging/udev/70-rpunithm-slider.rules \
  /etc/udev/rules.d/70-rpunithm-slider.rules
sudo udevadm control --reload-rules
```

随后重新拔插设备，使当前活跃本地会话获得 `uaccess` ACL。通过 SSH 或无 seat 的后台环境运行时，需要另行配置用户组权限，不应将设备开放为全局 `0666`。

临时诊断也可对**本次枚举得到的目标节点**授权：

```sh
# 将 /dev/hidrawN 替换为接口 2 的实际节点；拔插后可能失效。
sudo setfacl -m "u:$(id -un):rw" /dev/hidrawN
```

### 4. 启动真实桥接

终端 A：

```sh
./target/release/chuni-bridge-probe bridge
```

终端 B：

```sh
./target/release/chuni-bridge-probe read --require-live --duration-secs 10
```

正常情况下，读端在确认心跳变化后进入 `state=live`，真实 HID 数据显示为 `source=2`。触摸后应出现非零压力，松手后恢复为零。`--require-live` 只要求观察到有效在线快照，不代表已经验证触摸、通道方向或全部传感器。

`bridge` 默认开启 LED 转发；仅测试输入时可使用 `bridge --no-leds`。无设备或无权限时，桥接进程保持离线并重试，因此进程仍在运行不等于设备已经在线。按 `Ctrl+C` 正常停止时会发布离线、全零的硬件输入快照，并保留共享文件。

## Wine 侧 DLL 构建与配置

### 构建 DLL

将 `WINE_ROOT` 替换为实际 Wine 安装目录。该目录需要包含 `include/wine/windows` 和 `include/wine/msvcrt` 等开发文件；仅有 `wine` 可执行文件不够。

```sh
WINE_ROOT="/absolute/path/to/wine"
sh tools/build_chuniio.sh "$WINE_ROOT"
```

构建产物：

```text
target/chuniio/x86/chuniio.dll
target/chuniio/x86/chuniio-probe.exe
target/chuniio/x64/chuniio.dll
target/chuniio/x64/chuniio-probe.exe
```

脚本使用现有 Clang / LLVM 工具构建 DLL 和 ABI 消费端，不依赖 MinGW，也不会自动安装工具链。

### 接入加载器

1. 停止游戏，备份现有 DLL 和配置文件。
2. 根据**加载 DLL 的进程位数**选择 x86 或 x64 产物，而不是根据 Linux 系统位数选择。
3. 将 DLL 放入加载器可访问的位置，并按所用 segatools / 加载器版本配置 chuniio DLL 路径。
4. 确认 Wine 前缀能够访问 Linux 共享文件，然后启动原生 `bridge` 和游戏。

不同加载器的 DLL 路径配置项可能不同。下面仅展示**本项目 DLL 自身读取的配置**，不能替代加载器的 DLL 加载配置：

```ini
[chuniio]
shmPath=Z:\dev\shm\chuni_bridge_probe

[io3]
test=0x31
service=0x32
coin=0x33
```

默认配置文件为进程工作目录下的 `segatools.ini`。上述虚拟键码对应键盘 `1`、`2`、`3`，分别用于测试、服务和投币；将对应值设为 `0` 可禁用该键。

| 配置方式 | 用途 |
| --- | --- |
| `CHUNI_BRIDGE_INI` | 指定 DLL 读取的 INI 文件路径，使用 Wine 可识别的路径 |
| `[chuniio] shmPath` | 指定共享文件的 Wine 路径 |
| `CHUNI_BRIDGE_PATH` | 覆盖共享文件路径，优先于 INI 中的 `shmPath` |
| 原生程序 `--path` | 指定 Linux 侧共享文件路径；需与 DLL 指向同一文件 |

DLL 以**读写方式**映射文件：读取输入并发布灯光。因此，即使只使用输入，也必须保证 Wine 进程对共享文件具有读写权限。`Z:` 并非所有前缀都存在；沙箱或自定义盘符环境需要调整映射和路径。

## 命令参考

所有子命令均可通过 `--help` 查看完整参数。

| 命令 | 用途 | 是否直接访问 HID |
| --- | --- | --- |
| `hid-list` | 枚举目标设备；`--all` 显示所有 HID 项 | 仅枚举，不轮询输入 |
| `hid-read` | 读取报告并显示原始通道与游戏通道顺序 | 只读输入 |
| `bridge` | 真实 HID 输入桥接，默认同时转发 LED | 读取输入；可写 LED 输出 |
| `produce` | 发布可校验的合成输入，不读取真实设备 | 否 |
| `read` | 观察共享内存输入，可选观察 LED 区 | 否 |
| `hid-led` | 直接向设备发送 LED 校准图样 | 写 LED 输出 |
| `led-emit` | 向共享内存发布合成 LED 图样 | 否；运行中的 bridge 可将其转发至设备 |

`bridge` 的常用参数：

| 参数 | 默认值 | 说明 |
| --- | --- | --- |
| `--path` | `/dev/shm/chuni_bridge_probe` | 共享文件路径 |
| `--retry-ms` | `500` | 设备缺失或读取失败后的重新枚举间隔 |
| `--report-timeout-ms` | `100` | 无合法 HID 报告时的离线阈值 |
| `--duration-secs` | `0` | 运行时长；`0` 表示持续运行 |
| `--device-path` | 不指定 | 诊断或多设备场景下固定选择一个枚举路径 |
| `--no-leds` | 不启用 | 禁止 LED 转发 |
| `--trace` | 不启用 | 逐快照记录输入，日志量较大，不含卡号字段 |

默认自动重发现不依赖固定的 `hidrawX`。显式设置 `--device-path` 后，节点重编号可能导致无法恢复。

## LED 调试

### 直接校准硬件

以下命令会实际改变设备灯光。校准前停止游戏和原有 `bridge`；若需保留输入桥接，请使用 `bridge --no-leds`，避免两个 HID 输出写者互相覆盖。

```sh
# 点亮区号 7，红色，保持 3 秒
./target/release/chuni-bridge-probe hid-led --zone 7 --color ff0000 --hold-ms 3000

# 彩虹动画，持续 10 秒
./target/release/chuni-bridge-probe hid-led --rainbow --hold-ms 10000

# 全部熄灭
./target/release/chuni-bridge-probe hid-led --off
```

已验证设备的区号 `0` 为最右端，`30` 为最左端，`31` 不对应物理灯。区号 `7`、`27`、两端、整条蓝色和彩虹动画已有操作员确认，但完整逐点对应关系仍需校准。

### 观察共享内存灯光

停止游戏 / DLL 的 LED 发布后，可在两个终端分别执行：

```sh
# 终端 A：发布合成灯光；若只观察共享内存，不要同时运行 LED 转发
./target/release/chuni-bridge-probe led-emit --solid 00ff00

# 终端 B：确认收到 LED 发布
./target/release/chuni-bridge-probe read --led --require-led --duration-secs 5
```

**颜色约定：** 当前游戏路径在 DLL 中将每组 `[G,B,R]` 一次性转换为设备输出 `[B,R,G]`，Linux 原样转发，不再做颜色轮换。`hid-led` / `led-emit` 的校准图样按 `RRGGBB` 参数生成，不经过 DLL 的重排；不能用校准命令的字节语义推断游戏接口的颜色顺序。

LED 区必须保持单写者：不要同时运行多个游戏进程，或让 `led-emit` 与游戏 DLL 同时发布。Linux 合成发布者之间有区间锁，但 Wine DLL 不参与该锁。

## 共享内存与故障保护

当前协议为 **v2**，共享文件固定为 **4096 字节**。输入区和 LED 区各自使用原子字段及 seqlock 快照；跨进程原子约定仅针对同机 x86 / x86_64 环境。

| 保护机制 | 行为 |
| --- | --- |
| 报告格式校验 | 仅接受恰好 45 字节的输入报告，不猜测或自动剥离 Report ID |
| 设备报告失效 | 默认 100 ms 无合法报告时清零、关闭句柄并重试；检测还受约 20 ms 读取粒度及系统调度影响，不承诺严格上限 |
| 进程失活 | 读端默认 300 ms 未观察到心跳变化后，硬件输入归零 |
| 首次映射 / 会话变化 | 先确认心跳继续变化，避免将旧文件中的非零输入误判为当前输入 |
| 不一致快照 | 有界重试，无法获得一致数据时不保留旧按压 |
| 正常停止 | 发布离线、全零输入，保留文件和 inode，便于已有读端等待恢复 |
| 单输入生产者 | 文件锁阻止多个 `bridge` / `produce` 同时写同一输入区 |

这里的硬件归零不代表禁用独立的键盘测试、服务或投币功能。进程心跳也不会延长旧 HID 报告的有效期。

**不要在任何读写端仍运行时删除、替换或缩小共享文件。** 否则原有映射与新进程可能访问不同 inode。协议不匹配或需要清理时，先退出游戏、DLL 消费端、桥接及诊断进程，再删除：

```sh
# 仅在确认所有相关进程已经退出后执行
rm -- /dev/shm/chuni_bridge_probe
```

### 数据与权限

- 新建共享文件默认权限为 `0600`；Linux 写端要求文件归当前用户所有，并拒绝跟随符号链接。
- 压力数据取自输入报告 `report[2..34)`；卡号字节不会进入输入快照或 `bridge --trace` 日志。
- `hid-read --raw` 会输出完整 HID 报告，可能包含卡号等敏感字段。分享日志前应检查并脱敏。
- `tools/capture_hid.py` 仅保存空气感应、按钮和压力样本，适合低噪声采集。
- 项目使用 hidraw 后端，不通过 libusb 解绑设备，也不自动修改系统权限。

## 测试与验证

### 本地检查

```sh
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

现有 Rust 单元测试覆盖报告解析、通道排列、快照一致性、LED 报告组装、心跳失效及部分桥接状态机行为。通过这些测试不等于完成真实硬件或 Wine 验收。

### 无硬件输入自检

先完成 release 构建。下面使用独立临时文件，不影响默认桥接路径，也不会访问 HID：

```sh
TEST_DIR="$(mktemp -d)"
./target/release/chuni-bridge-probe produce \
  --path "$TEST_DIR/input" --duration-secs 8 > "$TEST_DIR/producer.log" &
PRODUCER_PID=$!

./target/release/chuni-bridge-probe read \
  --path "$TEST_DIR/input" --verify-synthetic --duration-secs 5

wait "$PRODUCER_PID"
# 读写进程均退出后再清理
rm -- "$TEST_DIR/input" "$TEST_DIR/producer.log"
rmdir -- "$TEST_DIR"
```

合成在线快照满足 `slider[i] = (heartbeat + i) mod 256`。`--verify-synthetic` 要求至少观察到一个心跳确认的在线快照，校验失败返回非零退出码；**不要将该选项用于真实 HID 输入**。

### Wine 双位数验证

以下流程使用项目独立测试前缀 `artifacts/wine-prefix`，不使用默认或游戏前缀。先设置实际的 `WINE_ROOT`，并确保它包含所需开发文件和双位数运行支持：

```sh
WINE_ROOT="/absolute/path/to/wine"
cargo build --locked --release
sh tools/build_winmap_probe.sh "$WINE_ROOT"
sh tools/build_chuniio.sh "$WINE_ROOT"

mkdir -p artifacts
WINEPREFIX="$PWD/artifacts/wine-prefix" WINEARCH=win64 WINEDEBUG=-all \
  WINEDLLOVERRIDES='mscoree,mshtml=' "$WINE_ROOT/bin/wineboot" -i

# 合成输入：PE32 / PE32+ 文件映射、停止与恢复
python3 tools/validate_wine.py --wine "$WINE_ROOT/bin/wine"

# 双位数 DLL ABI、输入回调与 LED 发布
python3 tools/validate_chuniio.py --wine "$WINE_ROOT/bin/wine"
```

上述两项使用合成数据，不访问 HID。日志和 JSON 结果保存在被 Git 忽略的 `artifacts/` 中。**Wine 及双位数探针验证通过，不代表实际游戏前缀、其他运行器或全部游戏功能已通过。**

其他可选工具：

| 工具 | 用途与边界 |
| --- | --- |
| `tools/capture_hid.py` | 持续采集真实输入并汇总，默认调用 release 程序 |
| `tools/validate_hid_wine.py` | 真实 HID 到双位数 PE 读端的快照关联验证；会启动真实 bridge，默认启用 LED 转发，运行前应停止现有桥接 |
| `tools/measure_chuniio_latency.py` | 合成共享内存发布到 DLL 回调再到宿主输出的观测；包含采集开销，不是触摸到显示或 USB 端到端延迟 |

真实触摸或拔插验证需由操作员知情配合。`validate_hid_wine.py --reconnect` 需要新建的 `--arm-dir` 和明确放行；不是自动拔插工具。物理断线、节点重编号和权限恢复尚未完成验收，不能以状态机测试替代。

## 常见问题

| 现象 | 排查方式 |
| --- | --- |
| 找不到 `hidapi-hidraw` | 安装对应开发包，确认 `pkg-config --modversion hidapi-hidraw` 成功 |
| `Permission denied` | 重新枚举接口 2，检查当前节点的 ACL / udev；不要改用 root 运行 |
| 有多台匹配设备 | 从本次 `hid-list` 输出选择 `--device-path`；拔插后重新确认 |
| 已连接但一直离线 | `connected` 不代表收到合法报告；用 `hid-read` 检查长度、权限和是否有输入，查看超时或格式错误日志 |
| `another producer holds the file lock` | 检查是否已运行另一个 `bridge` 或 `produce`，不要通过删除文件绕过锁 |
| 协议 v1 / 版本不匹配 | 停止所有使用者后删除旧共享文件，再以同版本程序和 DLL 启动 |
| Wine 读不到数据 | 确认 DLL 位数、DLL 加载配置、共享路径、读写权限，以及 Wine 是否可访问同一宿主文件 |
| 灯光颜色异常或闪烁 | 检查是否重复转换颜色，或同时运行了游戏、`led-emit`、`hid-led` 等多个灯光写者 |

## 项目结构

```text
.
├── Cargo.toml                   # Rust 包与依赖
├── Cargo.lock                   # 锁定依赖版本
├── src/
│   ├── lib.rs                   # IPC 布局、原子快照、解析与 LED 报告组装
│   ├── main.rs                  # CLI 与共享内存读端
│   ├── native.rs                # HID 枚举、读取与合成输入
│   └── native/
│       ├── bridge.rs            # 真实输入桥接、重发现与 LED 转发
│       └── led.rs               # LED 发布与硬件校准命令
├── windows/                     # chuniio DLL、导出表与输入 / LED 状态逻辑
├── tools/                       # 构建脚本、采集、Wine 验证与延迟观测工具
├── packaging/udev/              # 目标设备权限规则
├── artifacts/                   # 本地日志与验证产物（不纳入版本控制）
└── target/                      # Rust、DLL 和 PE 构建产物（不纳入版本控制）
```

## 贡献与授权

提交问题时，请提供发行版、Rust / Wine 版本、设备枚举信息、所用 DLL 架构、复现步骤及脱敏日志。涉及输入或灯光协议的修改，应区分单元测试、合成链路验证和操作员实机确认，不应将其中一种结果描述为完整硬件验收。
