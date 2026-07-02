# Server Sponge — Architecture & Compatibility

## 一、工作原理

### 1. 内存占位（Memory Sponge）

```
┌─────────────────────────────────────────────────┐
│  PID 控制器（每 ~1s 一个 tick）                   │
│                                                  │
│  误差 = target - current_usage                   │
│      ↓                                           │
│  PID 输出 → chunk 数量（allocate/release）        │
│      ↓                                           │
│  MemoryPool（Vec<Chunk>）执行分配/释放             │
└─────────────────────────────────────────────────┘
```

**核心流程**（`controller.rs: tick()`）：

1. **读内存状态** — 优先 cgroup v2 `memory.current`，回退 `/proc/meminfo`
2. **PSI 压力检测** — epoll 监听 `/proc/pressure/memory`（非阻塞 poll，timeout=0）
3. **模式判定** — 状态机：`Steady ↔ Responsive ↔ Panic ↔ Cooldown`
4. **执行动作** — PID 计算 chunk 数，或 Responsive/Panic 按比例释放
5. **Swap 安全检查** — swap 使用率 > 10% 且增长 > 1% 时释放 30% 池

**状态机**：

| 模式 | 触发条件 | 行为 |
|---|---|---|
| **Steady** | 正常 | PID 微调，±5 chunks/cycle max |
| **Responsive** | PSI some | 释放 20% 池，重置积分 |
| **Panic** | PSI full 或 `available ≤ panic_threshold` | 释放 80%+全部 → 进入 Cooldown |
| **Cooldown** | Panic 后 N 秒 | 禁止分配，等待时间到 → Steady |

**PID 控制器**（`pid.rs`）：

- 非对称增益：释放方向 Kp = 2× 分配方向（快速退让，慢速恢复）
- 抗积分饱和（Anti-windup）：池为空且误差为负时停止积分
- 输出限幅：`[-limit, +limit]`（默认 ±100%）

**内存分配**（`memory.rs`）：

```
Chunk::allocate(size, huge):
  if huge → mmap(MAP_HUGETLB)  # 2MB 大页
   失败 → vec![0u8; size]      # 自动回退
  
  # 页面激活（RSS）
  madvise(MADV_POPULATE_WRITE)  # Linux ≥ 5.14
  fallback: 逐页写 0xAB          # 旧内核
```

### 2. CPU 占位（CPU Sponge）

```
┌─────────────────────────────────────────────┐
│ 控制线程（每 ~100ms 一个 cycle）              │
│                                              │
│ 读 CPU 使用率 → PID 计算占空比 → 写入 Atomic  │
│                                              │
│                ↓ (共享 duty AtomicU64)        │
│                                              │
│ N 个工作线程（SCHED_IDLE 调度）               │
│   busy_work(duty * cycle) + sleep(剩余)      │
└─────────────────────────────────────────────┘
```

**CPU 测量策略**：

| 条件 | 数据源 | 公式 |
|---|---|---|
| cgroup v2 可用 | `cpu.stat usage_usec` | `Δusage / (Δwall × cpus) × 100` |
| 无 cgroup | `/proc/stat` + `/proc/self/stat` | `(1 - Δidle/Δtotal) × 100` |

**避让机制**：

- **SCHED_IDLE**：工作线程优先级最低，内核自动让 CPU 给其他进程
- **Panic Yield**：当 `others_pct > target - margin` 时，占空比强制归零
- **Nice 10**：主进程整体优先级下调

### 3. Web Dashboard

- **Axum 0.7** HTTP 服务器，单线程 tokio runtime
- **SSE** (`/api/events`)：每秒推送实时指标
- **REST** (`/api/metrics`)：JSON 快照
- **stress-ng 集成**：面板上可直接启动/停止 CPU/内存压力测试

### 4. 进程优先级保护（`main.rs: lower_process_priority()`）

```
启动时自动设置：
  /proc/self/oom_score_adj = 800   # OOM 时优先被杀
  setpriority(PRIO_PROCESS, 0, 10)  # nice 10，低 CPU 优先级
```

之前只在 systemd 服务模式下生效，v0.3.0 起 `run` 命令也自动设置。

### 5. 隐身模式（`main.rs: stealth_init()`）

```python
prctl(PR_SET_NAME, name)           # 改 /proc/PID/comm（15 字符）
prctl(PR_SET_MM, ARG_START, ptr)   # 改 /proc/PID/cmdline（需 CAP_SYS_RESOURCE）
```

无 root 时仅 comm 生效，cmdline 不变。

---

## 二、操作系统/内核兼容性

### 使用的系统调用

| 系统调用 / 接口 | 最低内核版本 | 用途 | 降级策略 |
|---|---|---|---|
| `/proc/meminfo` | 2.6.0 | 内存信息 | 无替代，非 Linux 不可用 |
| `/proc/stat` | 2.6.0 | CPU 总体使用率 | 无替代 |
| `/proc/self/stat` | 2.6.0 | 进程 CPU 时间 | 无替代 |
| `epoll_create1` | 2.6.27 | PSI 事件监听 | — |
| `epoll_ctl` / `epoll_wait` | 2.6.0 | PSI 事件监听 | — |
| `sched_setscheduler` | 2.6.0 | 设置 SCHED_IDLE | 失败时警告，继续运行 |
| **SCHED_IDLE** | **2.6.23** | CPU 低优先级调度 | 自动降级为普通调度 |
| `/proc/pressure/memory` | **4.20** | PSI 内存压力 | `--no-psi` 回退轮询 |
| `MADV_POPULATE_WRITE` | **5.14** | 高效页面激活 | 自动回退逐页写 |
| `MAP_HUGETLB` | 2.6.32 | HugePage 分配 | 失败时回退普通分配 |
| `prctl(PR_SET_NAME)` | 2.6.9 | 修改进程名 | 忽略错误 |
| `prctl(PR_SET_MM, ...)` | **3.11** | 修改 cmdline | 忽略错误 |
| cgroup v2 `cpu.stat` | **4.15** | 容器内 CPU 测量 | 回退 `/proc/stat` |
| cgroup v2 `memory.max` | **4.15** | 容器内内存限制 | 回退 `/proc/meminfo` |
| cgroup v1 | 2.6.24 | cgroup 兼容 | 回退 `/proc/meminfo` |

### 最低内核要求

| 场景 | 最低内核 |
|---|---|
| 仅内存占位（无 PSI） | **2.6.23+**（SCHED_IDLE） |
| 内存占位 + PSI | **4.20+** |
| 内存占位 + PSI + MADV_POPULATE | **5.14+** |
| CPU 占位 | **2.6.23+**（SCHED_IDLE） |
| 容器环境（cgroup v2） | **4.15+** |
| 隐身模式（完整） | **3.11+**（+ root） |

### 实测兼容矩阵

| 操作系统 | glibc | 内核 | 兼容性 |
|---|---|---|---|
| Ubuntu 24.04 | 2.39 | 6.8 | ✅ 全功能 |
| Ubuntu 22.04 | 2.35 | 5.15 | ✅ 全功能 |
| Ubuntu 20.04 | 2.31 | 5.4 | ✅（无 MADV_POPULATE） |
| Debian 12 (Bookworm) | 2.36 | 6.1 | ✅ 全功能 |
| Debian 11 (Bullseye) | 2.31 | 5.10 | ✅（无 MADV_POPULATE） |
| CentOS 8 / Rocky 8 | 2.28 | 4.18 | ⚠️ 需 `--no-psi`，无 MADV_POPULATE |
| CentOS 7 | 2.17 | 3.10 | ❌ 内核 < 4.20，需 `--no-psi` 且 SCHED_IDLE 基本可用 |
| Alpine 3.20 (musl) | musl 1.2 | 6.6 | ✅ musl 构建 |
| openEuler 22.03 | 2.34 | 5.10 | ✅ |

### 非 Linux 系统

当前仅支持 Linux（大量使用 epoll、/proc、SCHED_IDLE 等 Linux 特有接口）。

---

## 三、musl 兼容性评估

### 使用的 libc 函数清单

| 函数 | glibc | musl | 状态 |
|---|---|---|---|
| `signal()` | ✅ | ✅ | 兼容 |
| `sched_setscheduler()` | ✅ | ✅ | 兼容 |
| `epoll_create1()` | ✅ | ✅ | 兼容 |
| `epoll_ctl()` | ✅ | ✅ | 兼容 |
| `epoll_wait()` | ✅ | ✅ | 兼容 |
| `close()` | ✅ | ✅ | 兼容 |
| `sysconf(_SC_CLK_TCK)` | ✅ | ✅ | 兼容 |
| `geteuid()` | ✅ | ✅ | 兼容 |
| `malloc_trim()` | ✅ | ❌ **不存在** | **已处理** `#[cfg(target_env = "gnu")]` |
| `mmap(MAP_HUGETLB)` | ✅ | ✅ | 兼容 |
| `munmap()` | ✅ | ✅ | 兼容 |
| `madvise()` | ✅ | ✅ | 兼容 |
| `prctl()` | ✅ | ✅ | 兼容 |
| `setpriority()` | ✅ | ✅ | 兼容 |

### musl 结构体差异

| 结构体 | glibc | musl | 处理 |
|---|---|---|---|
| `sched_param` | 1 字段 (`sched_priority`) | 5 字段（多 SCHED_SPORADIC 相关） | **`std::mem::zeroed()`** 按字段初始化 |

### musl 行为差异

| 特性 | glibc | musl | 影响 |
|---|---|---|---|
| **`malloc_trim`** | 归还缓存页到 OS | 不存在；大块 mmap 分配 free 时自动 munmap | 功能等价，无需改动 |
| **`Vec<u8>` ≥ 128KB** | 可能走 mmap，free 后缓存 | 大于 `MMAP_THRESHOLD` 时走 mmap，free 立即 munmap | musl 更及时地归还内存 |
| **线程局部存储** | 快速 | 略慢 | 对本工具无影响 |
| **数学库精度** | 高 | 略低 | 对本工具无影响 |
| **二进制大小** | ~3.3 MB (LTO) | ~3.4 MB (LTO) | 几乎无差异 |

### musl 构建状态

```
✅ cargo build --release --target x86_64-unknown-linux-musl  通过
✅ cargo build --release --target aarch64-unknown-linux-musl  通过（zigbuild 交叉编译）
✅ cargo test --target x86_64-unknown-linux-musl              192/192 通过
✅ 二进制完全静态，无 glibc 依赖
```

### 已知的 musl 限制

1. **无 `malloc_trim`** — 已通过条件编译处理，不会编译失败
2. **`sched_param` 结构体不同** — 已通过 `std::mem::zeroed()` 兼容
3. **DNS 解析** — 本工具不涉及
4. **线程创建速度** — 本工具线程数固定，非频繁创建

### musl 构建产物

```
server-sponge-linux-amd64-musl  — 完全静态，零依赖，可在任何 Linux 上运行
server-sponge-linux-arm64-musl  — 同上，ARM64
```

---

## 四、关键数据流

```
CLI 参数 / TOML 文件
  │
  ▼
Config (validate → apply_cli_overrides)
  │
  ├─▶ run_sponge()
  │     ├─ stealth_init()       # 隐身
  │     ├─ lower_process_priority()  # OOM + nice
  │     ├─ Controller::tick()   # 主循环（内存控制）
  │     ├─ CpuController        # 后台线程（CPU 控制）
  │     └─ HttpServer           # 后台线程（Web 面板）
  │
  ├─▶ install::install()        # systemd 安装
  └─▶ install::uninstall()      # systemd 卸载

Controller::tick() 循环 (1s):
  1. sysinfo::get_memory_info()
  2. psi.poll(0)                 # PSI 压力检测
  3. determine_mode()            # 状态机
  4. action_steady/responsive/panic/cooldown
  5. swap_safety_check()
  6. metrics_store.update()     # 推送到 Web 面板

CpuController 控制线程 (100ms):
  1. cpu_stat::read_*()          # CPU 使用率
  2. pid.update(error)          # PID 计算
  3. duty.store(new_duty)       # 工作线程读取
