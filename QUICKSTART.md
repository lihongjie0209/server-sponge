# Server Sponge 快速开始指南

## 这是什么？

Server Sponge 是一个**服务器资源占位工具**，可以将系统的内存和 CPU 使用率稳定在指定水平。当其他业务需要资源时，它会自动退让。

## 典型使用场景

| 场景 | 说明 | 推荐配置 |
| :--- | :--- | :--- |
| **压测环境模拟** | 模拟服务器高负载状态，验证业务在资源紧张时的表现 | `--target 80 --cpu-target 70` |
| **资源预留** | 预先占住资源，防止其他低优先级进程"偷吃"，需要时自动释放给核心业务 | `--target 60 --cpu-target 50` |
| **混部验证** | 验证多服务混合部署时的资源隔离和退让机制是否正常 | `--target 70 --cpu-target 70 --server-port 8080` |
| **容量规划** | 模拟不同资源水位下的系统表现，评估服务器实际可承载能力 | 根据需要调整 `--target` |
| **监控告警验证** | 人为制造高水位触发监控告警，验证告警链路是否正常 | `--target 90 --cpu-target 85` |

## 安装

### 方式一：从 GitHub Release 下载

```bash
# x86_64 服务器
curl -Lo /usr/local/bin/server-sponge \
  https://github.com/lihongjie0209/server-sponge/releases/latest/download/server-sponge-linux-amd64
chmod +x /usr/local/bin/server-sponge

# ARM64 服务器
curl -Lo /usr/local/bin/server-sponge \
  https://github.com/lihongjie0209/server-sponge/releases/latest/download/server-sponge-linux-arm64
chmod +x /usr/local/bin/server-sponge
```

### 方式二：从源码编译

```bash
cargo build --release   # 需要 Rust 1.85+
cp target/release/server-sponge /usr/local/bin/
```

---

## 三分钟上手

### 1. 只占内存（最简单）

将系统内存使用率维持在 70%：

```bash
server-sponge run --target 70
```

### 2. 只占 CPU

将系统 CPU 使用率维持在 50%（自动检测核心数）：

```bash
server-sponge run --target 0 --cpu-target 50
```

> `--target 0` 表示不占内存，只做 CPU 占位。

### 3. 同时占内存和 CPU（推荐）

```bash
server-sponge run --target 70 --cpu-target 50
```

### 4. 带监控面板

```bash
server-sponge run --target 70 --cpu-target 50 --server-port 8080
```

浏览器打开 `http://服务器IP:8080` 可实时查看资源曲线、PID 决策日志、触发压力测试。

### 5. 后台运行

```bash
# 方式 A：nohup
nohup server-sponge run --target 70 --cpu-target 50 &

# 方式 B：安装为 systemd 服务（推荐，开机自启、自动重启）
sudo server-sponge install --target 70 --cpu-target 50 --start
```

安装的 systemd 服务默认丢弃 stdout/stderr，不写入 `journalctl`；需要查看 journal 时显式添加 `--journal`：

```bash
sudo server-sponge install --target 70 --cpu-target 50 --journal --start
```

---

## 安装为系统服务

```bash
# 安装并立即启动
sudo server-sponge install --target 70 --cpu-target 50 --start

# 管理服务
systemctl status server-sponge          # 查看状态
systemctl stop server-sponge            # 停止
systemctl restart server-sponge         # 重启
journalctl -u server-sponge -f          # 查看实时日志

# 卸载（停止 + 删除服务 + 删除二进制）
sudo server-sponge uninstall
```

install 命令会自动完成：
- 复制可执行文件到 `/usr/local/bin/server-sponge`
- 生成 systemd 服务文件（包含你传入的所有参数）
- 设置 OOM 得分 +800（内核优先杀 Sponge 而非业务）
- CPU 占位启用时自动添加 `CAP_SYS_NICE` 权限
- `systemctl daemon-reload && systemctl enable`

---

## 完整参数说明

### 内存相关

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--target` | `70` | 目标内存占用率（%）。设为 0 禁用内存占位 |
| `--chunk-size` | `64` | 每次分配/释放的内存块大小（MB）。小值（16MB）精度高但操作频繁，大值（128MB）粗粒度但开销低 |
| `--panic-threshold` | `5` | 紧急释放阈值（%）。系统可用内存低于此值时清空全部内存池 |
| `--cooldown` | `30` | 熔断冷却时间（秒）。紧急释放后等待此时间再重新分配，防止震荡 |
| `--interval` | `1000` | 控制循环间隔（毫秒）。即每隔多久检查一次内存并做调整 |
| `--no-psi` | `false` | 禁用 PSI 内核压力监听。某些旧内核（<4.20）不支持 PSI 时需要加此参数 |

### CPU 相关

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--cpu-target` | `0` | 目标 CPU 使用率（%）。设为 0 禁用 CPU 占位 |
| `--cpu-workers` | `0` | CPU 工作线程数。0 表示自动检测（cgroup 限制或系统核心数） |
| `--cpu-cycle` | `100` | CPU 控制周期（毫秒）。每个周期内按占空比分配忙碌和休眠时间 |
| `--cpu-panic-margin` | `5` | CPU 避让余量（%）。当其他进程 CPU 占用 > target - margin 时，立即让出全部 CPU |

### PID 调参（通常不需要改）

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--kp` | `2.0` | 比例增益。越大响应越快，但可能震荡 |
| `--ki` | `0.1` | 积分增益。消除稳态误差，越大收敛越快 |
| `--kd` | `0.5` | 微分增益。抑制快速波动，越大越平滑 |

### 监控相关

| 参数 | 默认值 | 说明 |
| :--- | :--- | :--- |
| `--server-port` | `0` (禁用) | Web 监控面板端口。需显式指定端口号启用 |

---

## 工作原理（运维须知）

### 安全机制

| 机制 | 说明 |
| :--- | :--- |
| **SCHED_IDLE** | CPU 线程以最低优先级运行，只要有业务需要 CPU，内核会微秒级抢占 |
| **OOM Score +800** | systemd 服务模式下，内核 OOM Killer 优先杀 Sponge |
| **Nice 10** | 进程优先级低于默认值，业务进程优先获得 CPU 时间片 |
| **熔断释放** | 内存可用 < 5% 时立即清空全部内存池 |
| **Swap 监控** | 检测到 Swap 使用量增长时主动释放 30% 内存 |

### 自动退让行为

当业务进程启动或负载增加时：
1. **CPU**：SCHED_IDLE 让内核立即剥夺 Sponge 的 CPU 时间，同时 PID 降低占空比
2. **内存**：PSI 检测到压力后触发 Responsive 模式，快速释放 20%；如果压力持续升级则进入 Panic 模式清空全部内存
3. **恢复**：业务负载下降后，Sponge 通过 PID 算法缓慢回升到目标值（避免冲击）

### 日志说明

默认 `RUST_LOG=info` 级别，每秒输出 2 行聚合日志：

```
[MEM #  100] usage=69.8% target=70.0% pool=5×64MB mode=STEADY action=HOLD | avail=154 MB pid=+0.4
[CPU #  100] total=49.5% self=48.2% others=1.3% target=50.0% duty=48.6% action=PID
```

- `usage` / `total` — 当前使用率
- `target` — 目标值
- `pool` — 内存池状态（块数×大小）
- `mode` — 当前模式（STEADY/RESPONSIVE/PANIC/COOLDOWN）
- `action` — 本次操作（ALLOC/RELEASE/HOLD/YIELD）
- `duty` — CPU 占空比

需要更详细的日志时：

```bash
RUST_LOG=debug server-sponge run --target 70
```

---

## 常见问题

**Q: 需要 root 权限吗？**
A: `run` 不需要（除非启用 PSI 监听）。`install`/`uninstall` 需要 root。CPU 占位的 SCHED_IDLE 需要 `CAP_SYS_NICE`，非 root 时会自动降级但输出警告。

**Q: 内核太旧不支持 PSI 怎么办？**
A: 加 `--no-psi` 参数，程序会改用纯轮询模式，功能不受影响，只是响应压力会稍慢。

**Q: CPU 工作线程数应该设多少？**
A: 默认 `--cpu-workers 0` 自动检测，会匹配系统核心数（或 cgroup 限制）。一般不需要手动设置。

**Q: 如何验证退让机制正常？**
A: 开启监控面板 `--server-port 8080`，在面板上用 stress-ng 模拟压力，观察 Sponge 的内存/CPU 曲线下降。

**Q: 会影响业务进程吗？**
A: 几乎不会。CPU 使用 SCHED_IDLE（内核最低优先级），内存有 PSI 熔断机制 + OOM Score 优先被杀。设计目标就是"有空就占，有需就让"。
