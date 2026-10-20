# 团队线路管理 — 源码说明

线上地址：`https://38.58.57.22/`（管理后台 `/admin/v2/`，接口 `/v2/api/*`）

这是一套管理代理节点和线路机的内部面板，包括：首页与运营总览、节点详情、线路机、事项与告警、节点行为、套餐与续费、上架流水线、API 密钥、账户与安全。

本仓库只包含源码和文档，**不包含任何密钥、密码、令牌、数据库或线上配置文件**。部署所需的敏感信息需要你自己生成或从原服务器取得，见第 6 节。

---

## 1. 目录结构

```
团队线路管理-源码/
├── README.md                    ← 本文件
├── backend/fleet/               ← 中控后端 fleetd（Rust）
│   ├── fleetd/src/              ← 主程序：HTTP/SSE 接口、告警引擎、通知、存储、鉴权
│   ├── crates/health/           ← 健康检查与探测数据模型
│   ├── crates/probe/            ← 连通性/出口探测
│   ├── crates/ipintel/          ← IP 情报（归属地、数据中心判断）
│   ├── crates/onboard/          ← 上架流水线（新节点自动部署）
│   ├── deploy/                  ← 生产部署脚本、systemd 单元、配置示例
│   ├── staging/                 ← 测试服（隔离运行）配置与脚本
│   ├── .cargo/config.toml       ← 交叉编译到 musl 静态二进制的配置
│   ├── API.md                   ← 接口契约（62 KB，逐接口说明）
│   └── README.md                ← 后端模块说明（原有文档）
├── frontend/
│   ├── deploy-v2/               ← 前端构建脚本与实时数据适配层
│   │   ├── build.sh             ← 把前端打包成静态目录（产物 dist/，本包未附）
│   │   ├── live/                ← 实时数据适配：adapter.js、synth.js、boot.js
│   │   ├── tools/               ← 构建后校验（URL、CSP、真实 IP 规则）
│   │   └── staging/             ← 前端测试服
│   └── prototype/               ← 界面原型与页面源码
│       ├── original-refined/    ← 页面本体：index.html、css/、js/、assets/
│       ├── shared/              ← 共享组件与数据层（index.js 构建时生成）
│       ├── original/            ← 设计令牌（tokens.css）与字体
│       └── data/                ← 套餐目录 plan-catalog.json
├── edge/fleet-edge/             ← 边缘程序（运行在线路机和节点上）
│   ├── src/                     ← relayd（线路机代理状态上报）、nodeagent（主机遥测）
│   ├── fleetd-ingest/           ← 接收端的一部分
│   └── deploy/                  ← 边缘部署脚本
└── docs/
    ├── HANDOFF.md               ← 原交接说明
    ├── DECISIONS-autopilot.md   ← 架构与实施决策记录
    └── ops/fleet-prune-domains.py ← 域名记录每日清理（只保留 7 天）
```

---

## 2. 系统架构（先看这一节）

```
浏览器 ──HTTPS──► Caddy（中控 38.58.57.22）
                    ├─ /admin/v2/*        静态前端（来自 frontend/deploy-v2 的构建产物）
                    ├─ /v2/api/*          ──► fleetd 127.0.0.1:25900（REST + SSE）
                    ├─ /v2/ingest/*       ──► fleetd（边缘上报入口）
                    └─ /api/team-ops/*, /ops/api/*  ──► fleetd（兼容旧接口）

线路机（relayd） ──HTTPS 上报──► /v2/ingest/relay      （入口/出口/流量/探测结果）
主机（nodeagent）──HTTPS 上报──► /v2/ingest/node       （CPU、内存、磁盘、网卡）

fleetd 内部：
  - 数据库：SQLite legacy.db（WAL）。资产、线路机快照、事件、告警、审计都在这里
  - 告警引擎：alerts.rs + rules.rs，状态机 pending → firing → resolved，支持确认与静默
  - 通知：Telegram / 企业微信 / 邮件 / 签名 Webhook；未配渠道时为"影子模式"（只记录）
  - 上架流水线：onboard/，负责在新机器上部署代理服务并接入线路机
```

**两个关键点：**

1. **界面是静态页面**，由 Caddy 直接提供。页面通过 `/v2/api/*` 拉数据，再通过 `/v2/api/stream` 收实时推送。
2. **数据来自边缘上报**，不是后端主动去查。线路机上的 relayd 和主机上的 nodeagent 定时把状态推给 fleetd。旧版的 Komari 与 team-ops 后端已经下线，相关接口由 fleetd 原生渲染。

---

## 3. 环境要求

| 用途 | 需要 |
|---|---|
| 编译后端 | Rust 工具链（stable），`x86_64-unknown-linux-musl` 目标；系统 `gcc`（用于编译内置 SQLite） |
| 编译前端 | Node.js（`build.sh` 默认找 `~/.local/bin/node`，可用环境变量 `NODE` 指定） |
| 运行后端 | Linux x86_64，systemd，Caddy 2 |
| 测试服 | Python 3.12（`fleet/staging` 下的脚本） |

---

## 4. 构建

### 4.1 后端 fleetd

```sh
cd backend/fleet
cargo build --release
# 产物：target/x86_64-unknown-linux-musl/release/fleetd（静态链接，约 8 MB）
```

`.cargo/config.toml` 已经把目标固定为 `x86_64-unknown-linux-musl`，并让内置 SQLite 用系统 gcc 编译，关闭 LFS64 符号（musl 1.2.4 以后没有这些符号）。如果你的机器不是 musl 环境，需要先安装 `musl-gcc`，或者去掉 `.cargo/config.toml` 里的 `[build] target`。

### 4.2 前端

```sh
cd frontend/deploy-v2
./build.sh            # 输出 frontend/deploy-v2/dist/
```

`build.sh` 会把 `../prototype/original-refined` 等目录拼成一个静态目录，并做三件事：

- 为生产环境的内容安全策略（CSP）改写 `index.html`（把内联脚本抽成 `js/theme-init.js`）；
- 生成 `shared/index.js`，把原型的数据层接到实时适配器上（`live/adapter.js`）；
- 调用 `tools/dist.mjs check` 检查所有 URL、数据文件和 CSP 规则。

`WRITES=1 ./build.sh` 会打开写操作（需要后端写接口可用）。默认是只读预览。

### 4.3 边缘程序（线路机和节点上运行）

```sh
cd edge/fleet-edge
cargo build --release --bin relayd --bin nodeagent
```

（具体二进制名以 `Cargo.toml` 为准。）

---

## 5. 运行与部署

### 5.1 中控（fleetd）

1. 把编译好的 `fleetd` 放到 `/opt/fleetd/current/fleetd`。
2. 参考 `backend/fleet/deploy/fleetd.toml.example` 写 `/etc/fleetd/fleetd.toml`（**不含密钥的示例**，见第 6 节要填什么）。
3. 安装 systemd 单元：`backend/fleet/deploy/fleetd.service`，然后：
   ```sh
   /opt/fleetd/current/fleetd check-config --config /etc/fleetd/fleetd.toml
   systemctl enable --now fleetd
   ```
4. 导入套餐目录（幂等，内容不变则跳过）：
   ```sh
   fleetd import-catalog --file /opt/fleetd/current/catalog.json --config /etc/fleetd/fleetd.toml
   ```
5. 用 `deploy/caddy-route.sh` 的内容把路由加进 Caddy（`/v2/api/*`、`/v2/ingest/*` 转到 `127.0.0.1:25900`，`/admin/v2/*` 指向前端目录）。

### 5.2 前端上线

把 `frontend/deploy-v2/dist/` 整个目录放到 Caddy 配置的静态目录（线上为 `/var/lib/team-ops-ui/current`），然后访问 `https://<域名>/admin/v2/`。

### 5.3 边缘程序（线路机）

relayd 和 nodeagent 都是单独的 systemd 服务，配置文件示例在 `edge/fleet-edge/deploy/`。每个主机需要一个**令牌**，由中控签发：

```sh
fleetd agent-token issue <主机名> --name default --config /etc/fleetd/fleetd.toml
```

令牌写入主机的 `/etc/fleet-edge/token`（权限 600，属于运行用户），`agent.toml` 里写上 `endpoint`、`host_id`、`token_file`。`host_id` 必须和面板里资产名的前缀一致（例如 `团队2-01 · ATT · 1.2.3.4` 的 host_id 是 `团队2-01`），否则面板对不上号。

### 5.4 定时任务（运维）

- `docs/ops/fleet-prune-domains.py`：删除 7 天前的域名类行为记录。装成 systemd timer 每天 04:10 执行（单元示例见脚本注释）。
- `fleetd` 内部也有保留策略：已恢复告警 90 天、通知日志 90 天、审计 365 天、探测原始样本 24 小时。

---

## 6. 需要你自己准备的敏感信息（本包不包含）

| 内容 | 放在哪 | 说明 |
|---|---|---|
| fleetd 配置 `fleetd.toml` | `/etc/fleetd/fleetd.toml` | 参考 `fleetd.toml.example`。注意 `listen` 必须是 `127.0.0.1` |
| 账户密码 | 通过 `fleetd user set-password <名>` 设置 | 不要写进文件 |
| 旧后端只读密钥（若启用 legacy 适配） | 文件路径写在配置里 | 权限 0640 |
| Komari 系统 API 密钥（可选） | 同上 | 当前生产不用 |
| 通知渠道密钥（Telegram、企业微信、SMTP、Webhook） | 配置里写文件路径 | 未配置则保持影子模式 |
| 上架流水线的 SSH 密钥 | `/etc/fleet/onboard/id_ed25519` | 权限 0640，属 root:fleetd；用于登录新机器 |
| 边缘主机令牌 | 每台主机的 `/etc/fleet-edge/token` | 由 `agent-token issue` 签发，不要复用 |
| 数据库 | `/var/lib/fleetd/legacy.db` | 运行数据，本包不含。新环境从零开始，资产靠 `catalog.json` 导入 |

**文件里出现的路径（如 `/etc/fleet/onboard/id_ed25519`）只是位置，不是密钥本身。**

---

## 7. 数据与接口

- 接口契约：`backend/fleet/API.md`，逐接口列出路径、参数、返回字段。
- 实时推送：`/v2/api/stream`（SSE），资源内容哈希变化时推送，断线按 `Last-Event-ID` 补发。
- 常用接口：
  - `GET /v2/api/v1/alerts`：事项与告警
  - `GET /v2/api/v1/assets`：资产（节点）列表
  - `GET /v2/api/v1/quota`：配额
  - `GET /v2/api/v1/probes/latest`：最近探测结果
  - `GET /v2/api/behavior/...`：节点行为事件

---

## 8. 测试

```sh
cd backend/fleet
FLEETD_FIXTURES=<夹具目录> cargo test --release --no-fail-fast
```

- 单元测试覆盖告警状态机、续费日历、会话、SSE 补发、审计哈希链、保留期、通知影子模式等。
- `tests/replay.rs`：用真实线上数据重放告警场景。
- `tests/http.rs`：起一个假旧后端和假 Komari，做端到端 HTTP 测试。

**夹具（fixtures）不在本包中**，因为里面有真实 IP。需要的话自己准备脱敏后的样本。

测试服（隔离运行，不影响线上）：`backend/fleet/staging/`，入口脚本 `stage.sh`，支持注入故障场景：

```sh
backend/fleet/staging/stage.sh start
backend/fleet/staging/stage.sh inject '{"op":"node_fail","node":"t1-31"}'
backend/fleet/staging/stage.sh api /v2/api/v1/alerts
```

---

## 9. 安全注意事项

1. **本包不含任何凭据。** 但源码里有大量真实的运维逻辑（SSH 登录新机器、写入线路机配置、签发令牌）。拿到这份代码的人能看懂这些流程，所以请当作内部资料保管。
2. **前端 `admin` 页面依赖登录会话。** 写操作需要 `X-Fleet-Request: 1` 请求头和同源校验。不要把接口直接暴露到公网（只通过 Caddy 的 HTTPS 访问）。
3. **`auth.mode = "dev-allow-local"` 只能用于测试服**，生产必须是 `fleetd`。
4. **SSH 登录新机器的密钥** 和 **线路机配置写入** 是高风险操作。改动前先备份（脚本里都有 `onboard-backups/` 和 `.bak-*` 文件）。
5. **边缘令牌** 一旦泄露，任何人都能冒充主机上报数据。主机下线时用 `fleetd agent-token revoke <主机名>` 撤销。

---

## 10. 已知限制

- 前端的"节点行为"页面显示的保留天数是代码里写死的 14 天。实际域名记录现在由定时任务按 7 天清理，页面文字没有同步改。
- 新机器（ATTVPS 基础版）目前没有完整的遥测，只能看到入口和出口连通性，CPU 等指标已经由 nodeagent 上报，但资产记录需要手动维护。
- 测试服（staging）的 Komari 替身和旧后端替身只适用于本地测试，不能直接用在生产。
- 本包没有附带 `frontend/deploy-v2/dist/`（构建产物），需要用 `build.sh` 重新生成。

---

## 11. 快速上手（给新开发者）

1. 先读 `docs/HANDOFF.md` 和 `docs/DECISIONS-autopilot.md`，了解为什么这样设计。
2. 读 `backend/fleet/API.md` 和 `backend/fleet/README.md`，了解接口和模块。
3. 在本地跑测试服：`backend/fleet/staging/stage.sh start`，然后浏览器访问测试服地址。
4. 改前端：编辑 `frontend/prototype/original-refined/`，然后 `frontend/deploy-v2/build.sh` 重新生成，再用测试服验证。
5. 改后端：`cargo test` 通过后再编译、部署，部署前先备份 `/var/lib/fleetd/legacy.db`。

---

## 12. 仓库中未包含的数据

`frontend/deploy-v2/live/fixtures/` 和 `frontend/prototype/data/` 下的大部分 JSON 是线上数据快照（包含真实节点 IP 与客户节点信息），已经在 `.gitignore` 中排除，不进入仓库。`plan-catalog.json` 是构建需要的套餐目录，保留在仓库中。需要本地调试的话，可以向原负责人索取，或用脱敏数据自行生成。
