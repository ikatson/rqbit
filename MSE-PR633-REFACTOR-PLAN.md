# MSE PR #633 重构（已实施）：概要与决策记录

## 背景

rqbit PR #633 实现 MSE（消息流加密）。上游维护者 ikatson 提交 CHANGES_REQUESTED，本文件记录重构的完整梗概：维护者意见、已实施的改动、关键设计决策与验证结果。分支 `feat/mse-crypto-primitives`，单 commit `0402294c`（基于 `de2b107e`）。

## 一、维护者意见（CHANGES_REQUESTED）

主 review：**合并为单 commit**（"It's not too big, so let's just shrink into one commit"）+ 两大担忧：
1. **自写 crypto 太多**——应尽量用 crate（openssl 等，含硬件加速）
2. **spaghetti**——MSE 侵入 session/peer_connection 核心逻辑，建议抽象为 `connect_with_handshake` / `accept`

6 条 review comments：error.rs `MseForced` 去 SocketAddr、移除 Tcp-only 限制、去冗余行、RC4/DH 用 crate、session 抽象、uTP/socks 透明支持。

## 二、已实施的改动

### Phase 1 — 抽象层（解决 spaghetti）
- **出站**：`StreamConnector::connect_with_handshake()`（stream_connect.rs）——封装连接 + MSE 决策 + 握手发送，返回 `OutgoingHandshake { kind, read, write, mse_applied }`
- **入站**：`accept_with_handshake()` 自由函数 + `IncomingHandshake` 结构
- `session.rs`/`peer_connection.rs` 不再携带任何 MSE 细节（`IncomingOutcome`/`OutgoingOutcome`/`Sha1`/`AsyncReadExt` 全部收进 stream_connect）
- 移除 Tcp-only 限制：uTP/socks 透明支持 MSE
- `MseForced` 去 SocketAddr

### Phase 2 — 加密原语（解决自写 crypto）
- **DH-768 → `crypto-bigint`**：`Uint<12>` + `FixedMontyForm::pow`，删除手写 mod_reduce hack。MSE 固定 prime 硬编码（与 libtorrent/transmission/aria2 同值）。已验证外部 bigint 向量一致
- **RC4 → `rc4` crate**：
  - 删除自实现 `mse/rc4.rs`（140 行）
  - `Rc4Writer` 重写为**内循环方案**（参考 libtorrent `rc4_handler`）：`poll_write` 加密整 buffer → 存 pending → 循环 flush，返回 `Ok(full_len)` 或 `Pending`，**绝不返回短写**（消除双加密 bug）；Pending/短写密文缓存，重试不重复加密
  - `Rc4Reader` 用 rc4 crate
  - **PadB 扫描改 pattern-search**（libtorrent `read_pe_syncvc` / transmission `read_vc` 同款）：独立实例算 VC 密文模式 → 字节搜索 → 解密实际 VC 验证，不再需要 clone
- SHA-1 用现有 `sha1w`（crate + 可选硬件加速后端）

### Phase 4 — squash
- `git reset --soft de2b107e` → 单 commit `0402294c`，21 文件 +1843/−32

## 三、关键设计决策

### RC4 保留自实现 vs 用 crate 的决策过程
1. 最初倾向保留自实现：RustCrypto `rc4` crate **无 `Clone`**，无法实现 `Rc4Writer` 短写保护（Pending 重试需试探状态）
2. 调研发现：libtorrent/transmission **都不用 clone**——用 **pattern-search**（搜索加密后的 VC 密文模式）而非"试探解密"；`Rc4Writer` 短写用"加密与发送分离 + pending 缓冲"
3. 据此改用 rc4 crate：PadB 用 pattern-search，Rc4Writer 用内循环方案——**彻底绕开 clone 需求**

### 写错误处理（与三引擎一致）
libtorrent `disconnect(error, sock_write)`、transmission `call_error_callback`（非可重试）、aria2 `throw DL_RETRY_EX`（非 WOULDBLOCK）——**致命写错误一律断连**。rqbit 的 `poll_write` 对底层 `Poll::Pending` 返回 Pending（可重试，对应 EAGAIN），对 `Err` 传播（断连）。原测试 `write_error_does_not_advance_state`（错误后状态不推进）脱离真实场景，改为 `write_error_propagates`（错误传播 + sink 空）。

## 四、验证结果

- `cargo test -p librqbit --lib`：**45 passed / 0 failed / 5 ignored**
  - stream：pending/short-write 状态保持、写错误传播
  - mse：rc4 向量、dh768 外部向量、stream 包装、duplex 握手、明文嗅探回退、零长度 IA、分片前缀重放、fresh-redial、Disabled 单连接、Forced 失败、默认 Disabled 锁定
- `cargo check --all-targets`：0 error
- `cargo clippy`：新增改动 0 warning（剩余 5 个为基线既有 mod.rs cast/large-size 警告）

## 五、状态

- 单 commit `0402294c`（基于 `de2b107e`），工作区干净
- **未 push、未回复 PR**（按用户指示）
- 待办：回复维护者（含 pattern-search / 内循环方案说明）、force push
