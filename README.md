# Server Sponge — 动态资源占位与自动避险系统

基于 **PID 控制算法** 的智能资源占位工具，同时支持 **内存** 和 **CPU** 两个维度的动态占位。

- **内存占位**：配合 Linux PSI (Pressure Stall Information) 异步通知，精确维持目标内存占用率
- **CPU 占位**：通过 PWM 占空比调制 + SCHED_IDLE 内核调度，实现零干扰的 CPU 负载模拟
- **实时监控**：内置 Web 仪表盘，Chart.js 图表 + 日志流 + stress-ng 压力模拟控制

适用于压测模拟、资源预留、混部环境验证等场景。

## 特性

- 🎯 **PID 闭环控制** — 工业级 PID 算法分别控制内存和 CPU，非对称增益确保快速避让
- ⚡ **PSI 内核监听** — epoll 监听 `/proc/pressure/memory`，毫秒级感知内存压力
- 🔄 **PWM 占空比** — CPU 工作线程按 PID 输出的占空比在"忙碌/休眠"间切换
- 🛡️ **SCHED_IDLE** — CPU 工作线程以最低调度优先级运行，内核自动让路给业务进程
- 📊 **实时 Web 仪表盘** — 内存/CPU 使用率图表、PID 决策日志、模式状态、stress-ng 压力控制
- 📋 **详细决策日志** — 每个控制周期输出完整的 PID 计算、模式判定、操作决策（同步显示在仪表盘）
- 🔥 **压力模拟** — 仪表盘集成 stress-ng 控制，支持精确指定 CPU 负载百分比和内存占用量
- 🐳 **容器感知** — 自动检测 cgroup v2/v1 内存和 CPU 限制
- 🔧 **systemd 集成** — 一键安装为系统服务

## 架构概览

```
┌────────────────────────────────────────────────────────────┐
│                      Server Sponge                          │
├──────────────────────┬─────────────────┬───────────────────┤
│   内存控制 (主线程)   │ CPU 控制 (后台)  │ Web 监控 (后台)    │
├──────────────────────┼─────────────────┼───────────────────┤
│  /proc/meminfo       │  /proc/stat     │  axum HTTP Server  │
│  + cgroup memory     │  + cgroup cpu   │  + SSE 实时推送     │
│       ↓              │       ↓         │       ↓            │
│  PID Controller      │ PID Controller  │  Chart.js 仪表盘   │
│       ↓              │       ↓         │  日志流 + 压力控制  │
│  Chunk 分配/释放      │ Duty Cycle ×N   │       ↓            │
│  (malloc + activate) │ (work + sleep)  │  stress-ng 集成    │
│       ↓              │       ↓         │                    │
│  PSI 压力监听         │ SCHED_IDLE      │                    │
└──────────────────────┴─────────────────┴───────────────────┘
```

### 内存状态机

| 模式 | 触发条件 | 行动策略 |
| :--- | :--- | :--- |
| **稳定 (Steady)** | 压力正常 | PID 微调，缓慢增减内存块 |
| **响应 (Responsive)** | PSI `some` 信号 | 暂停分配，快速释放 20% |
| **熔断 (Panic)** | PSI `full` 或 可用 < 5% | 立即清空全部内存池 |
| **冷却 (Cooldown)** | 熔断后 30s 内 | 禁止分配，等待系统恢复 |

### CPU 双层防御

| 层级 | 机制 | 响应速度 |
| :--- | :--- | :--- |
| **应用层** | PID 调节占空比 + 熔断避让 | ~100ms（控制周期） |
| **内核层** | SCHED_IDLE 调度策略 | ~微秒（内核调度器） |

## 快速开始

### 编译

```bash
cargo build --release    # 需要 Rust 1.85+
```

### 仅内存占位

```bash
./target/release/server-sponge run --target 70 --chunk-size 64
```

### 仅 CPU 占位

```bash
./target/release/server-sponge run --target 70 --cpu-target 70
```

### 内存 + CPU 同时占位

```bash
./target/release/server-sponge run \
    --target 70 --chunk-size 64 \
    --cpu-target 70 --cpu-workers 4
```

### 安装为 systemd 服务

```bash
# 安装并立即启动（需要 root）
sudo ./target/release/server-sponge install \
    --target 70 --chunk-size 64 \
    --cpu-target 70 \
    --start

# 如需将服务日志输出到 journalctl，显式添加 --journal
sudo ./target/release/server-sponge install --target 70 --journal --start

# 仅安装，不启动
sudo ./target/release/server-sponge install --target 80 --no-psi

# 卸载
sudo ./target/release/server-sponge uninstall
```

`install` 命令会：
1. 复制可执行文件到 `/usr/local/bin/server-sponge`
2. 生成 `/etc/systemd/system/server-sponge.service`（包含所有参数）
3. 启用 CPU 占位时自动添加 `AmbientCapabilities=CAP_SYS_NICE`
4. 执行 `systemctl daemon-reload && systemctl enable`

### 管理 systemd 服务

```bash
systemctl status server-sponge         # 查看状态
systemctl start/stop/restart server-sponge
journalctl -u server-sponge -f         # 实时日志
```

## Docker 使用

```bash
# 构建
docker build -t server-sponge .

# 内存 + CPU + 监控仪表盘
docker run --rm --privileged -m 512m --cpus 2 -p 8080:8080 \
    server-sponge run --target 70 --chunk-size 16 \
    --cpu-target 70 --cpu-workers 2 --server-port 8080

# Docker Compose
docker compose up -d
docker compose logs -f sponge
```

## Web 实时监控仪表盘

启动时添加 `--server-port` 参数即可开启内置 Web 服务器：

```bash
server-sponge run --target 70 --cpu-target 70 --server-port 8080
```

浏览器访问 `http://localhost:8080` 即可打开监控仪表盘。

### 仪表盘功能

| 功能 | 说明 |
| :--- | :--- |
| **内存图表** | 实时显示系统内存使用率、目标线、Sponge 占比 |
| **CPU 图表** | 实时显示总使用率、其他进程负载、占空比、目标线 |
| **状态卡片** | 内存模式、池大小、可用内存、CPU 占空比、PID 输出、运行时间 |
| **压力模拟** | 通过 stress-ng 精确控制 CPU 负载 (线程数×负载%) 和内存占用 (MB) |
| **实时日志** | 所有 PID 决策日志实时显示，支持自动滚动和清空 |

### 压力模拟 (stress-ng)

仪表盘集成 stress-ng 压力工具，支持精确控制：

- **CPU 压力**：指定线程数、每线程负载百分比、超时时间
  - 例：2 线程 × 80% 负载 = 约 160% 总 CPU 负载（双核满载）
- **内存压力**：指定占用大小 (MB)、超时时间
  - stress-ng 会持续持有指定内存 (`--vm-keep`)

观察仪表盘可以清楚看到 Sponge 的**自适应避让行为**：
1. 启动 CPU 压力 → Sponge 降低占空比
2. 启动内存压力 → Sponge 释放内存块
3. 停止压力 → Sponge 缓慢回升到目标值

### API 接口

| 端点 | 方法 | 说明 |
| :--- | :--- | :--- |
| `/` | GET | 监控仪表盘页面 |
| `/api/metrics` | GET | JSON 格式当前指标快照 |
| `/api/events` | GET | SSE 实时数据流 (500ms 间隔) |
| `/api/stress/cpu/start` | POST | 启动 CPU 压力 `{"workers":2,"load":80,"timeout":60}` |
| `/api/stress/cpu/stop` | POST | 停止 CPU 压力 |
| `/api/stress/mem/start` | POST | 启动内存压力 `{"mb":200,"timeout":60}` |
| `/api/stress/mem/stop` | POST | 停止内存压力 |
| `/api/stress/stop` | POST | 停止所有压力 |

## 配置参数

### 内存参数

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--target` | 70 | 目标内存占用率 (%) |
| `--chunk-size` | 64 | 内存块大小 (MB) |
| `--panic-threshold` | 5 | 可用内存低于此值触发熔断 (%) |
| `--cooldown` | 30 | 熔断后冷却时间 (秒) |
| `--no-psi` | false | 禁用 PSI 监听 |
| `--kp` | 2.0 | 内存 PID 比例增益 |
| `--ki` | 0.1 | 内存 PID 积分增益 |
| `--kd` | 0.5 | 内存 PID 微分增益 |
| `--interval` | 1000 | 内存控制循环间隔 (毫秒) |

### CPU 参数

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--cpu-target` | 0 | 目标 CPU 使用率 (%, 0=禁用) |
| `--cpu-cycle` | 100 | 控制周期 (毫秒) |
| `--cpu-panic-margin` | 5 | 避让余量：其他进程 > target-margin 时全量让路 |
| `--cpu-workers` | 0 | 工作线程数 (0=自动检测 cgroup/nproc) |

### 监控参数

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--server-port` | 0 (禁用) | Web 监控仪表盘端口 (需显式指定端口号启用) |

## 日志示例

### 内存控制日志

```
[INFO] [#    5] ── Memory status: total=512 MB, used=321 MB, avail=190 MB (usage=62.8%) | pool=20 chunks
[INFO] [#    5]    Mode: STEADY (unchanged)
[INFO] [#    5]    PID compute: target=70.0% - current=62.8% = error=+7.23% Kp=2.0 P=+14.46 I=+19.26 D=-7.81
[INFO] [#    5]    Decision: ALLOCATE 3 chunks (wanted=8, max_to_target=3, cap=5)
```

### CPU 控制日志

```
[INFO] [CPU #   10] ── Status: total=65.3%, self=55.2%, others=10.1% | duty=78.5% | workers=4
[INFO] [CPU #   10]    PID: error=+4.7% Kp=0.8 P=+3.76 I=+2.1 D=-0.5 out=+5.36 | duty_delta=+0.054 → new_duty=83.9%
```

## 技术要点

### 内存管理
- **页面激活**：每 4KB 写入非零值，确保 RSS 真实分配
- **强制归还**：`libc::malloc_trim(0)` 绕过分配器缓存
- **非对称 PID**：释放 Kp = 2× 分配 Kp（安全偏向）
- **抗积分饱和**：池空且误差为负时停止积分累加

### CPU 管理
- **PWM 占空比**：100ms 周期内按比例分配忙碌/休眠时间
- **SCHED_IDLE**：Linux 最低优先级调度类，内核自动让路
- **自消除采样**：从 `/proc/stat` 和 `/proc/self/stat` 计算扣除自身后的他人负载
- **熔断避让**：当他人负载 > target - margin 时，duty 立即归零

### 安全机制
- **OOM 得分 +800**：内核 OOM Killer 优先终止 Sponge
- **Nice 10**：CPU 优先供给业务
- **Swap 监控**：Swap > 10% 时主动释放 30% 内存池

## 项目结构

```
server-sponge/
├── Cargo.toml
├── Dockerfile
├── docker-compose.yml
├── README.md
└── src/
    ├── main.rs          # 入口，CLI 子命令分发
    ├── config.rs        # 配置定义与校验
    ├── controller.rs    # 内存状态机控制器
    ├── pid.rs           # PID 控制器（内存/CPU 共用）
    ├── psi.rs           # PSI 压力监听 (epoll)
    ├── sysinfo.rs       # 系统内存信息 (cgroup/proc)
    ├── memory.rs        # 内存池管理
    ├── cpu_stat.rs      # CPU 统计 (/proc/stat 解析)
    ├── cpu_worker.rs    # CPU 负载生成 (PWM + SCHED_IDLE)
    ├── install.rs       # systemd 安装/卸载
    ├── server.rs        # Web 监控服务 (axum + SSE)
    ├── dashboard.html   # 实时监控仪表盘 (Chart.js)
    ├── metrics.rs       # 共享指标存储
    ├── stress.rs        # stress-ng 压力模拟管理
    └── log_capture.rs   # 日志捕获 (console + ring buffer)
```

## 测试

```bash
cargo test                    # 本地（需 Linux）
# 或通过 Docker
docker run --rm -v "$PWD:/app" -w /app rust:1.85-bookworm cargo test
```

当前 **185 个测试用例**，覆盖 PID 控制器、内存池、CPU 统计解析（含 cgroup v2）、配置校验、服务文件生成、日志捕获、压力管理等核心逻辑。

## License

MIT
