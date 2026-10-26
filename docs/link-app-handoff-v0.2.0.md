# Link 应用接入 linkSDK v0.2.0：共享存储锁竞争不再停用代理

本文件是接手实施指南，SDK 设计与验证范围以 [design.md](design.md) 为准。本次只发布 SDK；Link 的代理 BUG 仍需按下列步骤接入并复验，不因依赖升级就宣称已修复。

## 版本与兼容

私有仓库 `ZevaierZhan/linkSDK`，GitHub Release/tag `v0.2.0`。本包仍 `publish=false`，不是 crates.io 发布，也不包含 Link 安装件。

在有仓库读取权限的环境取得实际提交：

```sh
git fetch origin tag v0.2.0
git rev-parse 'v0.2.0^{commit}'
```

这里的命令在 **linkSDK 检出目录**运行。消费端将得到的完整 SHA 同时写入 `src-tauri/Cargo.toml` 的 `link-sdk.rev` 和 `scripts/link-sdk-lock.json`；执行 `node scripts/prepare-libzt.mjs`，再更新并提交 Cargo.lock。SDK 原生 checkout 与 Cargo 必须来自同一提交，不能混用 sibling checkout 或旧缓存。不要把本次依赖升级混入 Link 安装、真实家庭数据迁移或无关 UI 改动。

稳定身份、Planet、原 SQLite actor、schema v1 与 wire v2 保持。v0.2.0 在此前 Apple/iOS 兼容提交之上继续演进，保留 `action_in`、OS framed replication 与 generation gate。旧 `action_at/action_in/Dataset` 字符串接口仍可编译；**需要按故障类型处理的代理调用必须使用新 typed 接口，不要先转成 String 再匹配文字**。既有 replication 网络入口仍返回 String，保持兼容；代理的运行状态不应依赖背景同步轮次是否成功。

## SDK 已提供的接口

- `DataStore::open_at(home, &Operation)`：旧桌面/服务 profile 的 `space/space.json` 与数据库；由 host 继续负责 profile 生命周期。
- `DataStore::open_in(directory, network_id, &Operation)`：iOS 等 host 已认证的公共数据目录，无需伪造身份文件。
- `DataStore::action(request, &Operation)`，以及便于过渡的 `try_action_at/try_action_in`：`DataResult<Value>`，`DataError.kind` 与可选 SQLite extended code。
- `Operation::new(timeout).with_max_attempts(n)`：一次调用共享绝对截止时间；只自动重试 `Busy/Locked`，默认最多三次；`cancel()` 可由其他线程调用。每次新操作创建新的 Operation，不能长期复用已过期的对象。
- `DataStore::snapshot(&keys, &Operation)`：所有选中数据集同一只读事务；返回 `Arc<Snapshot>`，含 opaque `version`、`network_id`、`observed_at_ms`、每个数据集的 entries/heads。
- `store.reader(keys)?.refresh(&Operation)`：`Fresh { snapshot, changed }` 或 `Unavailable { last_good, error }`。失败不会替换最后成功快照，不会刷新旧快照的观察时间。
- `store.subscribe(keys, interval)`：有界一条事件队列；本进程提交唤醒、周期重读补偿跨进程/遗漏通知。慢消费者允许合并中间状态，不能把它当作不丢事件的日志。`recv_timeout` 消费事件，Drop 取消并加入 worker。

version 只可比较相等性，不代表单调序号、授权版本或“全网已同步”。`Fresh.changed=false` 仍是一次成功刷新，可用来清除临时错误；不能将 changed 当成唯一恢复信号。订阅的 changed 相对于最近成功进入事件队列的 Fresh；接收端仍应比较自身保存的 `snapshot.version`，并在任何 Fresh 上清除对应的临时读取错误。

## 具体修复位置与调用顺序

当前已确认的故障链在 `tunnel/service.rs::Manager::refresh_rules`：任何读取错误都会设置并保存 `preferences.active=false`，改变 settings/phase/revision；`coordinate` 每秒触发，`action(status)` 等路径也会触发。另一个问题是它持有 `Manager.state` 锁跨越数据库访问。请同时修两处，而不是仅增大 SQLite timeout。

1. 在 `shared_data.rs`/runtime 初始化后持有 DataStore，并增加保留 DataError 的 host adapter；直到 IPC 展示边界再格式化。身份/成员/generation 错误应保留 host 自身类别，不要伪装成 SQLite Busy。
2. 把代理共享配置读取集中到一个快照。至少包含 `link.proxy.v1/{rules,connect-rules,multi-rules,external-exits}` 与 `link.routing.v1/catalog`；其他使用到的共享数据也加入同一选择。修改 `rules.rs/routing.rs/external.rs` 的投影函数以接受此快照，不能取得快照后又偷偷调用旧 `shared_data::action` 重新读取。
3. 在持有 Manager.state 锁时只复制本机设置、runtime/generation 标识和当前配置版本，然后释放锁；数据库读取/重试、Automerge 解码与业务投影在锁外进行。
4. **先验证、后交换**：SDK Fresh 不等于代理业务配置有效。校验成员/空间/generation、目录 schema、冲突版本、选用/凭据/权限；再短暂重取 state 锁，核对 generation 与本地设置版本仍未改变，原子替换最后一份业务验证成功的配置。失败或过期任务的结果不能回写新 runtime。
5. 数据刷新状态与代理运行状态分开维护。例如 `catalogStatus = fresh/stale/unavailable`、最近成功版本/观察时间和暂时错误；`active` 保持用户本机意图。`status` 查询不应写停用配置。后台存储错误也不能令 TUN/helper 生命周期失败。
6. 数据订阅只触发配置重评估/标记脏，不能直接授予资源权限。保留定期修复读取和独立的撤权/租约检查；不能仅订阅后永久停止所有复核。

建议的 typed 使用入口（SDK 方法签名可直接编译；Proxy 的状态交换和策略由应用实现）：

```rust
use link_sdk::data::{DataStore, DatasetKey, Operation, SnapshotEvent};
use std::time::Duration;

fn read_configuration(home: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let store = DataStore::open_at(home, &Operation::new(Duration::from_secs(1)))?;
    let keys = vec![
        DatasetKey::new("link.proxy.v1", "rules"),
        DatasetKey::new("link.proxy.v1", "connect-rules"),
        DatasetKey::new("link.proxy.v1", "multi-rules"),
        DatasetKey::new("link.proxy.v1", "external-exits"),
        DatasetKey::new("link.routing.v1", "catalog"),
    ];
    let mut reader = store.reader(keys)?;
    match reader.refresh(&Operation::new(Duration::from_millis(100))) {
        SnapshotEvent::Fresh { snapshot, .. } => {
            // 在应用锁外进行完整业务校验，然后核对 generation 再交换。
            // 不能仅凭 SDK Fresh 就把 snapshot 标记为代理可用。
            let _candidate = snapshot;
        }
        SnapshotEvent::Unavailable { last_good, error } => {
            // last_good 仅为 SDK 数据快照，不保证通过代理校验。
            // 应用保留的是自己最近“业务验证成功”的配置；按下表处理。
            let _unavailable = (last_good, error.kind);
        }
    }
    Ok(())
}
```

## 错误与代理行为矩阵

| 情况 | 代理应用应采取的行为 |
|---|---|
| 刷新 Busy/Locked，已有业务验证成功配置，且成员/提供端许可/租约仍有效 | 标记 stale 并有界重试；不写 active=false、不改用户启用意图；既有流可在原授权范围内继续。新流是否允许由当前授权和本地策略决定 |
| 刷新 DeadlineExceeded | 本次结果不可用，按有界刷新失败处理；不要扩大为“已撤权”，也不能承诺下一次必成功 |
| Cancelled / generation 退休 | 丢弃该任务结果；按 host 的退休/关闭流程处理，绝不能借旧快照复活被撤销的 runtime |
| 首次启动无业务验证成功配置 | 不启用新代理；可以保留用户选用意图并提示数据暂不可用，不能用空配置冒充成功 |
| 明确成员移除、提供端撤销或租约过期 | 立即执行原权限策略，终止/拒绝相应流；SDK 旧快照不能延长授权 |
| 成功读取的新配置确实删除/改变规则，或存在 CRDT 冲突 | 沿用既有业务变更/冲突处理策略；它不是存储锁错误，不能假装没变化 |
| 写 Busy/Locked/Deadline/取消 | 不能显示保存成功，不把新草稿应用到运行配置，不停止原代理；SDK 没有偷偷排队待写操作 |
| StaleVersion | 重新读取并让用户处理过期表单；不能自动重放旧 expectedVersions，不能覆盖远端新值 |
| CapacityExceeded / DiskFull / ReadOnly | 明确提示未确认保存，保留草稿；不自动重试、不清空数据、不丢 actor；不要把用户代理意图改成停用 |
| CorruptStorage / UnsupportedFormat / SpaceMismatch / Io / StorageFailure | 不把错误解读为空目录或默认为缺省规则；按影响范围拒绝不可信的新配置并显式报告，不能无期限以缓存掩盖损坏。I/O 错误后若提交结果存在不确定性，重新读取核对，禁止盲目重放 |

默认 `is_retryable()` 只表示 Busy/Locked 可作为新的有限尝试，不证明业务授权仍有效。所有自动重试均受一次 Operation 的尝试数及绝对截止时间约束。取消/截止时间是 SQLite progress hook 和事务检查点协作执行；不承诺任意 OS I/O 或 Automerge CPU 工作可在硬实时期限内抢占。

## SDK 与应用各自的验收

SDK 复验（纯数据无需 native/vendor）：

```sh
cargo test --locked --no-default-features --features data
cargo run --locked --no-default-features --features data --example data-resilience
```

示例应输出 result=passed，并证明持有真实写锁时读快照成功、写入返回 Busy、不误报提交、解锁后恢复。`tests/data_contention.rs` 中父测试会实际启动独立子进程持锁/写入；唯一 ignored 项是供父测试显式调用的 subprocess fixture，不是跳过核心验收。

Link 接入后必须增加真实 Manager 纯存储 harness 的红绿回归：已启用并拥有有效配置→另一个 SQLite 连接/进程持写锁→status/后台刷新/保存分别执行→active 与本地意图不变、失败写不确认且原配置有效→释放锁自动读取新版本。再覆盖读取确实不可用的 Busy、并发共享同步、多应用写入、撤权/租约过期与缓存并发、首次无快照、CAS/CRDT 冲突、容量/满盘/损坏和 generation 退休。不要仅在 SDK 错误枚举上写断言就声称代理 BUG 已修。

遵守 Link To builder 的 daily-proxy-isolation：主工作机只做临时 profile/进程内 Manager/回环测试；系统 TUN/DNS/helper 及真实流量验证放独立测试机/VM。本次 SDK 发布未修改任何正式代理、家庭 profile、安装件或本机系统网络。应用真实修复、跨设备和 Windows/Linux/iOS 实机验收仍待消费端完成。
