# 上架流水线规格（旧实现逆向，供 v3 重写）

来源：`backend/fleet/crates/onboard/`（下称 `ob/`）。行号均指 2026-10-20 的 `v3-rewrite` 分支（commit a6c993a）。
文中「节点」= VPS 节点（住宅 IP 的 SS 出口机），「线路机」= 客户端入口机（Xray VLESS+REALITY，转发到节点的 SS）。
「中控」= 执行流水线的机器（38.58.57.22，同时是线路机2）。

---

## 0. 总览

旧流水线是一个 saga：每行一台新节点，16 个步骤 P1..P16（`ob/src/model.rs:7-24`），7 个阶段（`model.rs:27-35`）。
节点侧步骤逐行并发（默认 5）；P13（接入线路机）是批次级的，同一台线路机的就绪行合成一次 Xray API 变更（`engine.rs:198-304`）。
状态全部落 SQLite（`db.rs:14-57`），进程随时可杀，`resume` 从库里续跑。每一步「先观察后动作」，所以重跑幂等（`steps.rs:1-3`）。

**线路机本身不在流水线里被创建**：线路机必须事先由 fleet-cutover 切成 `fleet-xray`，并且切换已 COMMIT（`WIRING.md:11,119-125`）。本流水线只往已有线路机上追加/撤销入口。线路机开通属于旧系统的缺口（见 §9）。

### 0.1 机器侧副作用总清单（重写必须覆盖或有意丢弃）

节点（每台）：

| # | 对象 | 由哪步产生 |
|---|---|---|
| N1 | `/root/.ssh/authorized_keys` 追加上架公钥一行（0600，`~/.ssh` 0700 umask 077） | P2（仅密钥登录被拒时） |
| N2 | `/etc/ssh/sshd_config` 首行可能被插入 `Include /etc/ssh/sshd_config.d/*.conf`；新建 `/etc/ssh/sshd_config.d/00-fleet-keyonly.conf`；sshd reload | P7 |
| N3 | 若无 `nft` 则 `apt-get install -y -q nftables` | P7 |
| N4 | `/etc/sysctl.d/90-fleet-bbr.conf`；`modprobe tcp_bbr`；`sysctl -p` 使 fq+bbr 立即生效。**v1**：基线不再管 BBR，并入调优档（tune-v1 起，现为 tune-v2，见 DESIGN.md §3「系统调优」；收敛里基线后紧跟执行，写 `/etc/sysctl.d/99-zz-fleet-tune.conf`、journald 体积上限，补时间同步；脚本顺手删掉旧的 90-fleet-bbr.conf），见 DESIGN.md §3 | P7 |
| N5 | `/etc/fleet/nft/ss_guard.nft`；内核里的 `table inet ss_guard`；`/etc/nftables.conf` 末尾追加 include 行（文件不存在则建，首行 shebang）；`systemctl enable nftables` | P8 |
| N6 | `/usr/local/bin/ssserver`（0755，shadowsocks-rust 1.25.0 musl）；`/etc/shadowsocks-rust/config.json`；系统用户 `fleet-ss`；`/etc/systemd/system/fleet-ssserver.service`；enable + start | P9 |
| N7 | nodeagent 全套：用户/组 `fleet-edge`，`/opt/fleet-edge/**`，`/etc/fleet-edge/{agent.toml,token}`，`fleet-nodeagent.service` + drop-in，`/var/lib/fleet-nodeagent`（StateDirectory） | P11 |

线路机（每个入口）：

| # | 对象 | 由哪步产生 |
|---|---|---|
| R1 | Xray 运行态：1 个 inbound（`inbound-<port>`）、1 个 outbound（`ss-<ip>`）、1 条 routing rule（`r-inbound-<port>`），经 API 热加，不重启 | P13 |
| R2 | `/etc/fleet/xray/config.json` 同样追加这三个对象（原子改名，保持属主/权限） | P13 |
| R3 | `/etc/fleet/xray/onboard-backups/config.json.<时间>.<pid>`（每次改配置前 `cp -p`，**从不清理**） | P13/detach |
| R4 | 锁文件 `/run/lock/fleet-xray-config.lock`（flock 时创建） | P13/detach |

中控（本地状态）：SQLite `onboard.db`（0600，含明文机密）、`work_dir/known_hosts/<row_id>`、`work_dir/askpass.sh`、`work_dir/bin/ssserver`（解包）、`work_dir/secrets/<batch>-<uuid>.pw`（网页提交的引导密码，0600，**从不删除**，`api.rs:192-204`）、`/tmp/fonb-cm-$USER/`（ssh ControlMaster 套接字，`ssh.rs:36-47`）。

---

## 1. 输入（每台机器一行）

### 1.1 CSV 格式

`ob/src/intake.rs:1-9`，前端 `onboarding-live-dialog.js:14`：

```
provider,ip,ssh_port,auth,plan,team,number[,relay,ss_port,dd,mac,service_id,note]
```

- 表头必需，列顺序随意，不区分大小写（`intake.rs:166-171`）；缺必需列 = 整批报错返回（`intake.rs:172-174`）。
- 解析器极简（`intake.rs:121-147`）：逗号分隔，`"` 包裹 + `""` 转义，`#` 开头行与空行忽略，`\r` 去除。不处理 BOM（前端上传时去 BOM，`onboarding-live-dialog.js:188`）。
- 前端额外限制：CSV ≤ 60 KB（`MAX_CSV`，js:17）；**表头里出现 `password` 列直接拒绝**（js:22）；至少一行数据。后端并不拒绝 `password` 列（只是忽略它）。

### 1.2 逐列校验（`intake.rs:159-310`）

| 列 | 规则 | 失败 |
|---|---|---|
| provider | 自由文本；与套餐的 `provider_name` 忽略大小写比较（`:231-236`） | error |
| ip | 必须是 IPv4（`:188`） | error |
| ssh_port | 空 = 22；非数字或 0 = error（`:191-196`） | error |
| auth | `key` 或 `password:<文件>`（也认 `password=`）；相对路径相对 CSV 所在目录；文件必须存在（`:197-210`）。API 创建时目录 = `work_dir/secrets`（`api.rs:124`） | error |
| plan | 见 §1.3 | error |
| ss_port | 空 = 8388（`cfg.ss.port`）；NAT 套餐必填（`:238-245`）；0/非数字 error | error |
| team | 纯数字 → `团队{n}`（`norm_team` `:149-156`）；其它原样；不能为空 | error |
| number | 正整数（`:247-250`） | error |
| relay | 空 = 自动放置；非空必须在 `[[relays]]` 配置里（`:251-256`） | error |
| dd | 空/`none` = 不重装；`manual` = 人工闸门；其它 error（`:257-264`） | error |
| mac | 小写化；可选 | – |
| service_id / note | 可选，仅展示 | – |

唯一性/冲突校验：
- 同批内 `(ip, ssh_port)` 不可重复（`:265-268`）；同批内 `(team, number)` 不可重复（`:269-271`）。
- 目录（catalog.json `assets`）里已有的 `(team, number)`：error（`:272-274`，`taken_numbers` `:94-105` 从 `assets[].short_name` 末尾 `-N` 解析）。
- 非 NAT 机的 IP 已在目录资产里：error（`:275-277`，`known_ips` 从 `asset_name` 按 ` · ` 取最后一段）；**NAT 机跳过**（共享 IP）。
- 其他批次已登记的 `(team, number)`：error（`planner.rs:43-49`，本批自己的除外，保证重跑 plan 稳定）。
- 同一 `(ip, ssh_port)` 同时只能有一个进行中的行：DB 唯一索引 `rows_live_ip`，条件 `state NOT IN ('active','cancelled')`；failed 不释放（`db.rs:26`）。
- 资产唯一：`(team, number)` 与 `(relay, relay_port)` 都是 `lifecycle <> 'retired'` 条件的唯一索引（`db.rs:52-53`）。

### 1.3 套餐目录查找

`Catalog::resolve`（`intake.rs:75-91`）顺序：① `plans[].id` 精确等于；② `plans[].canonical_name` 规范化后相等（`norm_alias`：trim、小写、全角括号→半角、折叠空白，`:59-62`）；③ `alias_index[]` 里 `provider`（小写）+ `alias_norm` 相等 → `plan_id`。别名只在同一供应商内匹配。

取用字段：`id`、`canonical_name`、`specs.nat`（默认 false）、`host_access`（默认 true）、`price.amount`（字符串）、`price.currency`（默认 USD）、`specs.bandwidth_mbps`、`provider_name`。`host_access=false`（代理产品，无主机）→ error「不走这条流水线」（`:228-230`）。

**重写注意**：
1. 后端目录 `architecture/plan-catalog/catalog.json` **不在仓库里**。仓库里只有前端投影 `frontend/prototype/data/plan-catalog.json`（schema `plan-catalog/1`），字段名不同：用 `name`/`provider`/`price`（字符串）/`specs.bandwidth_bps`，没有 `canonical_name`/`provider_name`/`alias_index`/`assets`，有 `by_asset`（短名→plan id）。其中的 plan：ATTVPS 基础版 `attvps.resi-vps.basic`（US、28.88 USD/月、nat=false）、ATTVPS 标准版、NodeMach 高速代理（host_access=false）、`ohnoicu.resi-nat-aca.small`（nat=true，MX）、GoMami Pulse Mini/Nano（category=relay）、菲律宾住宅+双ISP认证（host_access=false）。两个 schema 必须统一。`fleetd/src/catalog.rs:39-40,126` 读的是 `canonical_name` 形态。
2. 旧代码不看 `category`：一个 `category=relay`（GoMami）且 `host_access=true` 的套餐会被当成节点通过校验。重写应显式限定 `category=landing`。
3. 价格/带宽仅被抄进资产与展示，不影响任何机器侧动作。

### 1.4 供应商代号与命名

`provider_code`（`config.rs:223-233`）：配置 `[[providers]]` 优先（忽略大小写）；内置 ATTVPS→ATT、NodeMach→NM、OhnoICU→MX；其它取大写字母数字前 4 位。
命名：`short_name = {team}-{number:02}`（如 `团队1-37`），`name = {short_name} · {provider_code} · {ip}`（`intake.rs:278-280`）。`short_name` 同时是 nodeagent 的 `host_id`（`steps.rs:620`）。行 id = `{batch_id}-{seq}`（`db.rs:154`）。

### 1.5 NAT 处理

NAT 套餐（`specs.nat=true`）的差异，全部由 `ss_public_port` 驱动：
- 节点上 SS 始终监听本地 `8388`（`RowSpec.ss_port`，`intake.rs:288`）；供应商把公网端口 `ss_port` 映射到节点 8388；`ssh_port` 也是供应商映射的端口。
- 线路机 outbound 连 `ip:ss_public_port`（`steps.rs:137-141`）；outbound tag 带端口：`ss-<ip点换横线>-<ss_public_port>`，非 NAT 为 `ss-<ip点换横线>`（`relay.rs:39-42`）。
- nft 放行与丢弃的是**本地** 8388（`steps.rs:449-453` 用 `s.ss_port`），不是公网映射端口；假设供应商的端口转发保留来源 IP。（旧代码从未对真实 NAT 机验证这个假设，只在 `lab.sh` 里模拟。）
- P8 的正测/反测、P12 的出口核对都用 `ss_public_port`。
- 目录 IP 冲突校验对 NAT 机跳过；冲突键是 `(ip, ssh_port)`。
- 内存只有 256 MB 的 NAT 机：ssserver `MemoryMax=128M`、nodeagent `MemoryMax=24M`，没有任何内存预检（P3 采了 `mem_mb` 但不判断）。

### 1.6 编号与线路机放置

- 编号由人在 CSV 里给，系统不自动分配（只校验冲突）。
- 线路机放置 `place`（`intake.rs:326-354`）：负载 = 线路机现有入口数（配置里 tag≠`api` 的 inbound 数，`relay.rs:82-84`）+ 本批已显式分配的行；空 relay 的行依次放到「负载最少且 < `max_nodes`（默认 25）」的线路机；全满 → error；显式指定导致超限 → error（`:348-352`）。
- 线路机端口 plan 预览：`max(配置里 20001..20999 的端口)+1` 起连号（`planner.rs:79-84`），仅提示，实际以 P10 分配为准。

### 1.7 plan 阶段的只读线路机检查

`planner::plan`（`planner.rs:38-92`）对**每台配置的线路机**做一次 `relay::snapshot`（§4.1）；读失败 → 若有行落在该线路机则 error，否则 warn（`:57`）。若 `ip:ss_public_port` 已挂在该线路机某个 outbound 上，且不是本批同一行自己的 tag → error「现有节点」（`:62-72`）。

---

## 2. 选项、计划哈希与确认

- 默认选项（`planner.rs:33-36`、`config.rs:6-31`）：`canary=true`、`canary_gate="P16"`、`concurrency=5`、`relay_window_s=30`、`relay_batch_max=20`、`dry_run=false`。前端范围：并发 1–10，窗口 10–300 秒步长 5（js:196-197）。
- `plan_hash = sha256(json(rows) ‖ json(options))` 的前 16 个十六进制（`intake.rs:313-318`）。**rows 是放置之后的行**（含自动分配的 relay、密码文件路径），所以哈希依赖线路机当时的负载，不是 CSV 的纯函数。
- `batch_id = "b-" + sha256(csv_text)[..10]`（`intake.rs:320-323`）：同一份 CSV 得到同一批次，重复提交幂等（201 新建 / 200 已存在，`api.rs:133`）。
- 建批次要求 plan 无 error（`planner.rs:102-104`）；状态 `validated`。
- `run`（CLI）：重新 plan，错误则拒绝；`--confirm` 必须等于**库里已存的哈希**（批次已存在时）或新算的哈希（`fleetctl.rs:133-138`），然后 `ensure_batch` 才把新哈希写回（`planner.rs:96-101`）。
- `start`（API）：`body.plan_hash` 必须等于库里存的哈希，否则 409（`api.rs:143-149`）。

**弱点（重写应修正）**：确认哈希只证明「用户看过的是创建时的计划」，`start` 不会重算计划，所以创建后线路机负载/端口/冲突变化不会让哈希失配；前端文案「线路机状态或选项变了」（js:163）与后端行为不符。另外 `options` 里的 `canary_gate`、`relay_batch_max`、`dry_run` 被计入哈希，但引擎实际读的是全局配置（`engine.rs:353,392`）或根本不读，选项形同虚设。`resume --no-canary` 会直接改库里的选项而不改哈希（`fleetctl.rs:166-170`）。

---

## 3. 节点步骤 P1..P16

通用机制：

- **SSH**（`ssh.rs`）：系统 `ssh`，`-p <port> -o StrictHostKeyChecking=yes -o UserKnownHostsFile=<每行一个文件> -o GlobalKnownHostsFile=/dev/null -o ConnectTimeout=10 -o ServerAliveInterval=5 -o ServerAliveCountMax=3 -o LogLevel=ERROR`；密钥登录再加 `-i <ssh_key> -o IdentitiesOnly=yes -o BatchMode=yes`，ControlMaster=auto、ControlPersist=60、ControlPath=`/tmp/fonb-cm-$USER/%C`（`:49-69,110-121`）。脚本一律经 stdin 喂给远端 `bash -s`，不上命令行。用户固定 `root`（`steps.rs:116`）。
- **密码引导**（`ssh.rs:155-178`）：`setsid -w ssh … -o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive -o NumberOfPasswordPrompts=1`，`SSH_ASKPASS` 指向 `work_dir/askpass.sh`（内容 `exec cat "$FLEET_ONB_PWFILE"`），`SSH_ASKPASS_REQUIRE=force`、`DISPLAY=:0`、`FLEET_ONB_PWFILE=<密码文件>`；不复用连接。只对已钉住的主机公钥发送。
- **错误映射**（`ssh.rs:71-84`）：stderr 含 `REMOTE HOST IDENTIFICATION HAS CHANGED`/`Host key verification failed` → identity；退出码 255 且含 `Permission denied` → input；其它 255 → transient；超时 → transient（`:98`）。
- **推文件**（`ssh.rs:135-152`）：`cat > <remote>.fleet-tmp`（umask 077，mkdir -p 父目录）→ `sha256sum -c` → `chmod <mode>` → `mv -f`。超时 180 s。
- **失败分类与重试**（`model.rs:84-145`，`engine.rs:146-165`）：

| 类 | 行为 |
|---|---|
| transient、relay | 自动重试，最多 `retry_max=3` 次尝试，退避 `5s·2^(n-1)`（5 s、10 s）；用尽 → 行 `failed` |
| input | 不重试，步骤 `waiting_input`、行 `waiting_input`（等人补密码/确认闸门） |
| identity、conflict | 不重试，步骤 `blocked`、行 `blocked`（风险停止，`resume --row --force` 才能继续，`engine.rs:482-484`） |
| provider、bug | 不重试，行 `failed` |

- **逐步幂等**：`run_row` 看到某步 `succeeded/skipped` 就直接 advance（`engine.rs:129-133`）；步骤开头先查目标状态。

### P1 预检（`steps.rs:215-262`）

1. TCP 连 `ip:ssh_port`：最多 3 次，每次超时 5 s，间隔 0.5 s；不通 → transient。
2. `scan_stable("pre_dd")`（`:144-178`）：从中控 `ssh-keyscan -T 5 -p <port> -t ed25519 <ip>` 重复 `keyscan_samples=5` 次、间隔 `keyscan_interval_ms=3000`（每次进程超时 15 s）；每把观察到的公钥记入 `host_keys` 表。0 把 → transient；>1 把不同 → **identity**（疑似 IP 被冒名/ARP 冲突，不发密码）；恰好 1 把 → 采用。
3. 若 `known_hosts/<row>` 已钉住且与本次不同、且 P6 没有 `pinned` 记录 → identity（`:230-233`）。
4. 第二视角：在所属线路机上执行 `ssh-keyscan -T 5 -p <port> -t ed25519 <ip>`（20 s）；任一把与中控视角不同 → identity；线路机没有 ssh-keyscan 或连不上则跳过（不挡）。
5. `relay::snapshot(所属线路机)`；若 `ip:ss_public_port` 已在其 outbound 里且（tag 不是本行的 tag 或本行还没有资产）→ conflict「现有节点」。
6. 钉住公钥：写 `work_dir/known_hosts/<row_id>`，单行 `ip key`（非 22 端口用 `[ip]:port key`）。
输出：指纹、样本数、banner、第二视角结论。**中控是 TOFU，没有任何带外指纹。**

### P2 推密钥（`steps.rs:265-277,192-212`）

1. 用上架密钥试 `true`（20 s）；成功 → `method=already`。
2. `auth=key` 且无密码文件 → input（「把上架公钥加进 root 的 authorized_keys 后 resume，或补密码文件」）。
3. 用密码文件登录，远端执行（40 s）：
   ```
   umask 077; mkdir -p ~/.ssh; touch ~/.ssh/authorized_keys;
   grep -qxF '<pubkey>' ~/.ssh/authorized_keys || echo '<pubkey>' >> ~/.ssh/authorized_keys;
   chmod 600 ~/.ssh/authorized_keys; echo ok
   ```
   密码被拒 → input（「root 密码被拒…补密码后 resume --row … --password-file …」）；输出不含 `ok` → transient。
4. 再试密钥登录；失败 → transient。输出里带提醒「引导密码已用过：请在供应商面板重置 root 密码」（旧系统**不**轮换 root 密码）。
- 补偿：只在 P7 未成功时从 `authorized_keys` 删除该公钥行（`grep -vxF … > ak.tmp; mv`）；P7 成功后保留（`:866-874`）。

### P3 身份采集（`steps.rs:280-316`）

远端一次性采集（30 s）：`/etc/os-release` 的 ID/VERSION_ID、`/sys/class/dmi/id/product_uuid`、`ip -o link` 里所有 `link/ether` MAC、`/run/systemd/system` 是否存在、`systemctl --version`、`MemTotal`(MB)、`uname -r`、`uname -m`、`command -v nft`、`/etc/machine-id`。
通过条件：`arch == x86_64`（否则 provider，因 ssserver 包只有 x86_64）；CSV 给了 `mac` 则必须出现在 MAC 列表里（否则 identity）。其余只记录不判断（不检查发行版、内存、systemd 版本）。输出即 DD 前后比对的基线。

### P4–P6 重装（`steps.rs:321-391`）

仅 `dd=manual` 时生效，否则三步都 `skipped`。**没有任何自动重装**（供应商没有客户可用的 API；`bin456789/reinstall` 留给后续，`WIRING.md:130`）。
- P4：无 `ack` 时返回 `WaitInput`「请在供应商面板重装后继续…」；`resume --row [--password-file]` 把 `ack=true` 写进 P4 输出（`engine.rs:493-495`）后放行。
- P5：恒 `skipped`（「人工在供应商面板执行」）。
- P6：关闭 ControlMaster；轮询 TCP（每 5 s，上限 `dd_wait_timeout_s=1800` s，超时 → provider）；`scan_stable("post_dd")`（又是 5 次）；若指纹与 P1 相同且 P6 还没钉过 → `WaitInput`（「看起来还没重装」）；否则重新钉住新公钥并把 `pinned` 写进 P6 输出；密钥登录失败则再用（新）密码推公钥；再跑 `facts()`，比对 DMI product_uuid（两边都非空则必须相等，否则 identity），无 uuid 则比对 MAC 交集，都没有则「只认新钉住的主机公钥」。

### P7 系统基线（`steps.rs:394-440`，单脚本，超时 360 s）

远端脚本（`set -eu`）：
```
command -v nft >/dev/null || { export DEBIAN_FRONTEND=noninteractive; timeout 300 apt-get install -y -q nftables >/dev/null; }
install -d -m 755 /etc/ssh/sshd_config.d
grep -qE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*\.conf' /etc/ssh/sshd_config || sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
F=/etc/ssh/sshd_config.d/00-fleet-keyonly.conf       # 内容 5 行：
  # fleet onboard：只允许密钥登录（文件名 00- 保证先于 50-cloud-init 等生效）
  PasswordAuthentication no
  ChallengeResponseAuthentication no
  PermitRootLogin prohibit-password
  PubkeyAuthentication yes
（写 $F.tmp，与现有文件 cmp 相同则不动；不同则 mv 并标记 CH=1）
sshd -t
[ CH=1 ] && { systemctl reload ssh || systemctl reload sshd || kill -HUP $(cat /run/sshd.pid); sleep 0.5; }
sshd -T | grep -E '^(passwordauthentication|kbdinteractiveauthentication|challengeresponseauthentication|permitrootlogin|pubkeyauthentication) '
/etc/sysctl.d/90-fleet-bbr.conf = "net.core.default_qdisc=fq" / "net.ipv4.tcp_congestion_control=bbr"
modprobe tcp_bbr || true ; sysctl -q -p /etc/sysctl.d/90-fleet-bbr.conf || echo ==SYSCTLFAIL
cat /proc/sys/net/ipv4/tcp_congestion_control ; cat /proc/sys/net/core/default_qdisc
```
> **v1**：上面最后三行（BBR）已从基线脚本移走，归调优档（tune-v1 起，现为 tune-v2；`jobs/tune.rs`，步骤结果 `tune`）；非 bbr 只记警告，不失败。

通过条件：`sshd -T` 显示 `passwordauthentication no`；`permitrootlogin` ∈ {`without-password`, `prohibit-password`}（否则 bug）；`tcp_congestion_control == bbr`（否则 provider；仅容器演练 `require_bbr=false` 豁免）；若 CSV 给过密码文件，再用密码登录一次「反测」，必须被拒（`Permission denied`→input→记 `password_login_denied=true`），若 `LOGGED_IN` 出现 → bug。
注意：没有 `apt-get update`（陈旧索引会装不上）；不检查 `kbdinteractiveauthentication`；`default_qdisc` 只记录不判断；不改 root 密码、不改 SSH 端口、不装 fail2ban/unattended-upgrades、不设主机名/时区。无补偿（保留）。

### P8 防火墙（`steps.rs:443-497`，脚本 60 s）

ruleset 全文（`allow` = 所属线路机的 `allow_ips`，逗号拼接；`{p}` = 本地 SS 端口 8388）：
```
table inet ss_guard
delete table inet ss_guard
table inet ss_guard {
  set relays { type ipv4_addr; elements = { <allow_ips> } }
  chain input {
    type filter hook input priority -10; policy accept;
    tcp dport 8388 ip saddr @relays accept
    udp dport 8388 ip saddr @relays accept
    tcp dport 8388 drop
    udp dport 8388 drop
  }
}
```
远端动作（`set -eu`）：base64 解到 `/etc/fleet/nft/ss_guard.nft.tmp` → `nft -c -f`（语法检查）→ `mv -f` → `nft -f`（「声明 → 删 → 重建」单事务，幂等）→ 持久化：`/etc/nftables.conf` 不存在则 `printf '#!/usr/sbin/nft -f\n'` 建出，`grep -qF 'include "/etc/fleet/nft/ss_guard.nft"'` 没有则追加到**末尾**（Debian 默认以 `flush ruleset` 开头）→ systemd 下 `systemctl enable nftables`（不 start）→ `nft list set inet ss_guard relays`。
检查：输出里必须含每个 allow IP（否则 bug）；`allow_ips` 为空 → bug。
**正测**：从所属线路机执行 `timeout 4 bash -c 'exec 3<>/dev/tcp/<ip>/<ss_public_port>'; echo rc=$?`；`rc=124`（超时）→ provider（供应商侧防火墙）；0（已连上）或 1（RST）都算过。
**反测**：选一台「不是所属线路机、且其 `public_host` 不在 allow_ips」的线路机做同样连接，必须 `rc=124`；没有这样的线路机则从中控直连 4 s 超时，要求超时。不满足 → bug。故 `onboard.toml` 至少配两台线路机（`WIRING.md:135`）。
SSH 端口 **没有**任何防火墙限制（对全网开放，靠仅密钥）。`allow_ips` 是按线路机配置的，一个节点只放行自己所属线路机；节点换线路机必须改 nft（旧系统没有这个操作）。
补偿：`nft delete table inet ss_guard; rm -f /etc/fleet/nft/ss_guard.nft; sed -i '\#include "/etc/fleet/nft/ss_guard.nft"#d' /etc/nftables.conf`（不 disable nftables.service，不删 `/etc/nftables.conf`）。

### P9 Shadowsocks（`steps.rs:500-555`）

- 二进制：中控启动批次前 `prepare_ssserver`（`:27-49`）校验 tarball 的 sha256 与旁边 `.sha256` 文件，`tar --no-same-owner --no-same-permissions -xJf <tarball> -C work_dir/bin ssserver`；配置路径 `/var/lib/fleet/dist/shadowsocks-v1.25.0.x86_64-unknown-linux-musl.tar.xz`。节点上 `sha256sum /usr/local/bin/ssserver` 不同才 `push_file(…, "/usr/local/bin/ssserver", "0755", sha)`（180 s）。
- 密码：**每台 32 字符**，字母表 `ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789`（56 个，去掉 I O l 0 1），`rand::thread_rng()`（CSPRNG）；存 `secrets(row_id,'ss_password')`，`INSERT OR IGNORE` 保证重跑不换（`steps.rs:96-100,134-136`，`db.rs:435-443`）。
- 配置 `/etc/shadowsocks-rust/config.json`（`serde_json::to_string_pretty` + 换行）：
  ```json
  {"server":"0.0.0.0","server_port":8388,"password":"<32字符>","method":"aes-256-gcm","mode":"tcp_and_udp","timeout":300}
  ```
  method 来自 `[ss].method`（默认 `aes-256-gcm`）。仅 IPv4 监听。远端先比 sha256，不同才写（`.tmp` + chmod 600 + mv）。
- systemd 分支（`/run/systemd/system` 存在）：建系统用户 `useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-ss`；配置 `chgrp fleet-ss; chmod 640`（即 0640 root:fleet-ss）；单元 `/etc/systemd/system/fleet-ssserver.service`：
  ```
  [Unit]
  Description=fleet shadowsocks-rust ssserver
  After=network-online.target
  Wants=network-online.target
  [Service]
  ExecStart=/usr/local/bin/ssserver -c /etc/shadowsocks-rust/config.json
  Group=fleet-ss
  User=fleet-ss
  Restart=always
  RestartSec=2
  LimitNOFILE=65536
  MemoryMax=128M
  NoNewPrivileges=yes
  ProtectSystem=strict
  ProtectHome=yes
  PrivateTmp=yes
  [Install]
  WantedBy=multi-user.target
  ```
  内容变了才 mv + `daemon-reload`；`systemctl enable`；配置/二进制变了或不在运行才 `restart`。
- 非 systemd（仅容器演练）：`setsid /usr/local/bin/ssserver -c … >> /var/log/fleet-ssserver.log &`，自带 pgrep/kill。生产机不需要保留此分支。
- 检查：最多 30×0.2 s 轮询 `ss -Hltn "sport = :8388"` 与 `ss -Hlun "sport = :8388"` 都有监听；否则 transient。输出 `ssserver --version`、配置文件权限。脚本超时 90 s。
- 补偿：`systemctl disable --now fleet-ssserver; pkill -f '^/usr/local/bin/ssserver -c …'; rm -f 单元 /usr/local/bin/ssserver; rm -rf /etc/shadowsocks-rust; daemon-reload`（不删 `fleet-ss` 用户）。

### P10 登记（`steps.rs:558-594`，纯中控/库内 + 一次线路机只读快照）

1. `relay::snapshot` 取线路机当前配置。
2. 端口分配 `allocate_port`（`db.rs:452-476`）：本行已有 `reserved/active` 预留就复用；否则 `next = max(库里该线路机已分配的最大值, 配置文件里区间内的最大端口, lo-1) + 1`，区间 `20001–20999`；`> 20999` → conflict「端口段用完了」。刻意用 max+1 而不是最小空闲：**绝不复用被删掉的端口**，避免旧分享链接误连新节点。
3. 生成机密（各自 `secret_or_insert`，重跑不变）：`client_uuid`（UUID v4）、`reality_private_key`（32 随机字节 X25519，base64url 无填充）、`short_id`（8 随机字节 → 16 位 hex）；`reality_public_key` 由私钥推出后存入。
4. email（Xray 统计键）：`node-{NN:02}`；若该线路机配置里已有同 email，则 `t{团队数字}-{NN:02}`（如 `t1-37`）；已登记过的行沿用旧 email（`:571-581`）。**只对照配置文件里的现有 email，不看同批其它行**——同批里编号相同、团队不同的两行可能都拿到 `node-05`。
5. 资产 uuid（与 client_uuid 不同的另一个 `uuid_v4`，已有则沿用），写 `assets`（`lifecycle=onboarding`；冲突 → conflict）。
输出含 `relay_port`、三个 tag、email。补偿：删资产、`allocations` 中本行的 `reserved` 置 `retired` 并解绑（端口永不复用）。

### P11 装 nodeagent（`steps.rs:607-733`，`agent.rs`）

需要 `[agent]` 配置与令牌后端，否则 `skipped`。与 `edge/fleet-edge/deploy/rollout.sh` 的节点安装逐项一致（要求布局字节级相同，之后可用 rollout.sh 升级）。

1. **预检（只读）**（`agent.rs:228-250`，30 s）：`uname -m`、systemd 版本与是否在运行、cgroup 类型（`stat -fc %T /sys/fs/cgroup`）、os、到 `endpoint` 主机:443 的出站（`timeout 6 bash -c 'exec 3<>/dev/tcp/…'`）、`/etc/fleet-edge/token` 的 sha256、已有版本目录 nodeagent 的 sha256、`boot_id`、`/etc/fleet-edge/agent.toml` 里的 `host_id`、是否有 `fleet-relayd.service`、`runuser`、可用内存。
   判定：arch≠x86_64 → provider；systemd 未运行或 <239 → provider；无 runuser → provider；装着 relayd → conflict；agent.toml 已登记为别的 host_id → conflict；出站不通 → provider；无 boot_id → transient。
2. **令牌**（`agent.rs:348-375`）：节点上令牌 sha 在 fleetd-ingest 的有效令牌里 → 沿用；否则若该 host_id 有有效令牌、且 300 s 内收到过 boot_id 不同的上报 → conflict（编号被在用节点占着）；否则（先撤销该 host_id 的孤儿令牌再）签发新令牌。签发后立刻把 `sha256(token)` 写进 P11 输出（明文不落库）。令牌格式 `fe1_…`，rollout.sh 的格式检查：去空白后 ≥20 字节、全可见 ASCII。命令模式（fleetctl 独立运行）没有 ingest 视图，只认「上次由本流水线签发的 sha」。
3. **用户与目录**（`agent.rs:253-262`，30 s）：`groupadd --system fleet-edge`；`useradd --system --gid fleet-edge --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin fleet-edge`；`install -d -m 0755 -o root -g root /opt/fleet-edge /opt/fleet-edge/releases /etc/fleet-edge`；版本目录 `/opt/fleet-edge/releases/<VERSION>-<sha8>`（0755）。
4. **二进制**：版本目录里 nodeagent 的 sha 不同才 `push_file(… "0755")`。安装包校验（`agent.rs:139-171`）：`<dist>/nodeagent` 的 sha256 必须等于 `<dist>/SHA256SUMS` 里的值、必须是 ELF、`VERSION` 字符合法（缺省 0.1.0）、单元文件含 `ExecStart=/opt/fleet-edge/current/nodeagent`；不符则整个批次不开始。
5. **安装脚本**（`agent.rs:275-313`，120 s；`set -eu; umask 022`）。每个文件「内容不同才替换并写 `/etc/fleet-edge/.restart`」：
   - `/etc/fleet-edge/agent.toml`（0644）：
     ```
     # 由 fleet-edge/deploy/rollout.sh 生成（内容不变就不重启）
     endpoint = "https://38.58.57.22"
     host_id = "<短名>"
     token_file = "/etc/fleet-edge/token"
     sample_interval_s = 5
     batch_samples = 2
     ```
   - 仅新签发时：`/etc/fleet-edge/token`（`umask 077; printf '%s\n' <token> > token.new; chown fleet-edge:fleet-edge; chmod 0600; mv`），令牌只在 ssh stdin 里。
   - `/etc/systemd/system/fleet-nodeagent.service`（内置，见 `assets/fleet-nodeagent.service`）：`User/Group=fleet-edge`、`ExecStart=/opt/fleet-edge/current/nodeagent --config /etc/fleet-edge/agent.toml`、`Restart=always RestartSec=10`、`StateDirectory=fleet-nodeagent`(0700)、`MemoryMax=24M MemorySwapMax=0 CPUQuota=3% TasksMax=8 Nice=10 IOSchedulingClass=idle OOMScoreAdjust=500 LimitNOFILE=256`，加固 `NoNewPrivileges CapabilityBoundingSet= ProtectSystem=strict ProtectHome PrivateTmp PrivateDevices ProtectKernel* ProtectControlGroups ProtectClock ProtectHostname RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK RestrictNamespaces RestrictRealtime RestrictSUIDSGID LockPersonality MemoryDenyWriteExecute RemoveIPC SystemCallArchitectures=native SystemCallFilter=@system-service ~@privileged SystemCallErrorNumber=EPERM`。
   - `…service.d/10-local.conf`：cgroup v2 为 `# 由 rollout.sh 生成`；非 cgroup2fs 加 `[Service]\nMemoryLimit=24M`。
   - **干跑**：`runuser -u fleet-edge -- <rel>/nodeagent --once --config /etc/fleet-edge/agent.toml`，要求退出码 0 且 stderr 含「令牌文件可读」，否则退出码 9（→ bug）。
   - `ln -sfn <rel> /opt/fleet-edge/current.new && mv -Tf …/current`（变了则标记重启）；`daemon-reload`；`systemctl enable`；有 `.restart` 或未运行才 `restart`（输出 `svc=restarted|unchanged`）；只保留最近 3 个版本（`ls -1dt /opt/fleet-edge/releases/* | tail -n +4 | grep -v ^<rel>$ | xargs -r rm -rf`）。
6. **等第一批**（`:697-723`）：每 3 s 查；fleetd 模式以 `host_latest` 为准（`received_at ≥ 本步开始` 且 `boot_id` 与节点一致）；否则节点 `/var/lib/fleet-nodeagent/status.json` 的 `last_ok ≥ 脚本里的 t0`。上限 `first_batch_timeout_s=90` s，超时 → transient（附 status.json 与 journalctl 最近 15 行），会自动重试（幂等）。
7. 补偿（`agent.rs:320-335`，**先**停/删节点文件，**再**撤销令牌，反过来 agent 会在被删前刷 401）：`systemctl disable --now`；删单元、drop-in、`/var/lib/fleet-nodeagent`、`/var/lib/private/fleet-nodeagent`；daemon-reload；装着 relayd 则只删 agent.toml，否则 `rm -rf /opt/fleet-edge /etc/fleet-edge`、`userdel/groupdel fleet-edge`；然后撤销该 host_id 全部令牌。

### P12 出站校验（`steps.rs:736-747`，`relay.rs:438-457`）

在**所属线路机**上用 Xray 二进制临时起一个进程：dokodemo-door 监听 UDS（`/dev/shm/fonb-chk.XXXXXX/s.sock`，不占端口），outbound = 该节点的 SS（tag 改 `out`，密码取库里的），再 `curl -sS -m 15 --unix-socket … https://www.cloudflare.com/cdn-cgi/trace`，取 `ip=`。要求 `ip == 节点 ip`；不等则**再起一个独立进程复测**，两次一致仍不等 → identity（「应答 8388 的可能不是这台机器」）。远端脚本超时 40 s。进程与临时目录由 `trap` 清理。

### P13 线路机接入

见 §4。引擎只在行到达 P13 时把它置 `ready_for_relay`，由批次级调度合批（`engine.rs:124-128`）。

### P14 探针与监测（`steps.rs:749-751`）

恒 `skipped`（「探测随入口自动派生（relayd 上线后生效）；今晚由 P15 主动验收代替」）。没有任何机器侧动作。

### P15 端到端验收（`steps.rs:754-787`）

在**中控本机**起 Xray 客户端（`client_xray` 路径）：dokodemo-door（UDS）→ VLESS REALITY outbound：`vnext = [{address: 线路机 public_host, port: relay_port, users:[{id: client_uuid, flow: xtls-rprx-vision, encryption: none}]}]`，`realitySettings = {fingerprint: chrome, serverName: sni, publicKey: reality_public_key, shortId: short_id}`，目的地 `www.cloudflare.com:443`；`curl -sS -m 15` 最多 3 次（间隔 1 s），要求 `ip=` 存在。整体超时 90 s。`ip == 节点 ip` 通过；是别的 IP → identity（「规则没生效会走线路机 IP」）；请求失败 → transient。注意：配置 JSON 经 `bash -c` 命令行（base64）传入，本机 `/proc/<pid>/cmdline` 可见。

### P16 上线交付（`steps.rs:790-802`）

纯库内：拼分享链接
```
vless://<client_uuid>@<public_host>:<relay_port>?encryption=none&flow=xtls-rprx-vision&security=reality&sni=<sni>&fp=chrome&pbk=<reality_public_key>&sid=<short_id>&type=tcp#<百分号编码的完整名称 "团队1-37 · ATT · <节点IP>">
```
存 `secrets(share_link)`，资产 `lifecycle=active`，端口分配 `reserved→active`。**注意：fragment 里带了节点真实 IP**，分享给客户端会泄露 VPS 节点 IP。`fleetctl onboard reveal` 输出链接。

---

## 4. 线路机侧

### 4.1 前置条件与连接

每台线路机在 `onboard.toml [[relays]]` 配置（`config.rs:172-203`，`deploy/onboard.central.toml:44-84`）：`name`、`host`、`ssh_port`、`ssh_user`(root)、`ssh_key`、`known_hosts`（**预先钉住、必填**）、`public_host`（分享链接用）、`allow_ips`（节点 nft 放行来源）、`xray_bin`(`auto`)、`asset_dir`(`auto`)、`config`(`/etc/fleet/xray/config.json`)、`api`(`127.0.0.1:62789`)、`sni`(`www.cloudflare.com`)、`max_nodes`(25)。生产三台：线路机1 23.148.204.129、线路机2 = 中控 38.58.57.22（经 `127.0.0.1` ssh 自己）、线路机3 38.58.57.69。
`xray_bin=auto`：`pgrep -o -f "run -c <config>"` 取运行中进程，`readlink -f /proc/<pid>/exe`（即 `/opt/fleet/xray/<sha16>/xray`），geo 文件目录取其所在目录（`relay.rs:110-120`）。Xray 配置里必须已启用 `HandlerService`、`RoutingService`（API inbound tag `api`），默认转发为 `direct`、有 geoip:private/bittorrent→blocked 规则（lab 形状，`WIRING.md:173`）。
与 fleet-cutover 共用排他锁 `flock -w 60 /run/lock/fleet-xray-config.lock`（环境变量 `FLEET_SPEC_LOCK` 可覆盖）；**不得与 cutover 的 harden/switch/rollback/relay-switch 并发**（`WIRING.md:119-125`）。

只读快照 `relay::snapshot`（`relay.rs:150-192`，40 s）：配置 sha256、Xray pid、`/proc/<pid>/stat` 第 22 字段（启动时刻）、`xray api statssys` 的 Uptime、运行态 `api lsi --isOnlyTags`/`lso`/`lsrules` 的 tag 集合、`ss -Htln` 监听端口、配置原文（base64 回传）。读不到配置/API 不通/运行态没有任何 inbound → `relay` 类错误。

### 4.2 入口对象模板（`relay.rs:46-63`）

「沿用旧 provision.py add_inbound 的参数，决定与 mihomo 客户端兼容性，不要改」。

```json
// inbound（tag = inbound-<port>）
{"listen":"0.0.0.0","port":<port>,"protocol":"vless","tag":"inbound-<port>",
 "settings":{"clients":[{"id":"<client_uuid>","email":"<email>","flow":"xtls-rprx-vision"}],"decryption":"none","fallbacks":[]},
 "streamSettings":{"network":"tcp","security":"reality",
   "tcpSettings":{"acceptProxyProtocol":false,"header":{"type":"none"}},
   "realitySettings":{"show":false,"xver":0,"target":"<sni>:443","serverNames":["<sni>"],
     "privateKey":"<reality_private_key>","minClientVer":"1.8.2","maxClientVer":"","maxTimeDiff":0,"shortIds":["<short_id>"]}},
 "sniffing":{"enabled":true,"destOverride":["http","tls","quic"],"metadataOnly":false,"routeOnly":false}}

// outbound（tag = ss-<ip点换横线>[-<ss_public_port> 仅 NAT]）
{"tag":"<otag>","protocol":"shadowsocks",
 "settings":{"servers":[{"address":"<node_ip>","port":<ss_public_port>,"method":"aes-256-gcm","password":"<ss_password>"}]}}

// routing rule（ruleTag = r-inbound-<port>）
{"type":"field","ruleTag":"r-inbound-<port>","inboundTag":["inbound-<port>"],"outboundTag":"<otag>"}
```
每个入口一个独立的 REALITY 密钥对、一个 UUID、一个 shortId；`sni`/`target` 取线路机配置（默认 `www.cloudflare.com`）。一个入口 = 一个节点（1:1），入口之间无共享对象。

### 4.3 `attach_batch`（`relay.rs:222-391`）：零重启热加

顺序（默认转发是 direct，先有入口后有规则会让流量从线路机 IP 出去，所以规则必须先于入口）：
**全部 AddOutbound → 全部 AddRule(append) → 端口检查 → 全部 AddInbound → 运行态核对 → 落盘**。

控制端（Rust）在锁外先做：
1. 重新 snapshot；对每个成员计算冲突（`:234-250`）：配置里同 tag 但内容不同的 inbound/outbound/rule；新入口端口已在配置里；新入口端口在系统里已被别的进程监听；目标 `ip:port` 已被别的 tag 的 outbound 占用。有冲突的成员单独标 conflict，不进本批。
2. 在候选配置副本里**追加**缺的对象（已存在且内容相同的跳过 = 幂等）。
3. `guard::append_only`（`guard.rs:31-126`）：顶层键集合不变、静态段不变、`routing` 除 `rules` 外不变；inbounds/outbounds/rules 必须是旧数组的**纯前缀追加**；新 inbound 必须 vless+reality、tag 非空唯一、端口唯一、`minClientVer ≤ 1.8.2`；新 outbound 必须 shadowsocks 且无 `proxySettings`；新 rule 只允许 `type/ruleTag/inboundTag/outboundTag` 四个字段、`inboundTag` 恰好是本批新增的一个入口、`outboundTag` 只能是本批新增的转发（不许 direct/已有转发/balancer）。失败 → bug。
4. 候选 JSON = `serde_json::to_string_pretty` + `\n`（与 fleet-cutover `jdump` 一致，已有对象字节不变），算 `cand_sha`。

远端脚本（stdin，120 s；`set -u; umask 077`）：
1. `exec 9>/run/lock/fleet-xray-config.lock; flock -w 60 9` → 失败 exit 20。
2. CAS：当前配置 sha256 必须等于读取时的 `EXPECT`，否则 exit 17（控制端重新快照重试，最多 3 次，`:224`）。
3. 候选写到 `/dev/shm/fonb.XXXXXX/cand.json`，`XRAY_LOCATION_ASSET=<asset> <xray> run -test -c cand.json` 必须通过（exit 18）。
4. 记 `P0/S0/U0`（pid、启动时刻、uptime），列出运行态 tag。
5. ① 每个出站：运行态没有该 tag 才 `xray api ado --server=127.0.0.1:62789 o.json`（文件内容 `{"outbounds":[ob]}`）；失败 exit 21。
6. ② 每条规则：没有才 `xray api adrules --server=… -append r.json`（内容 `{"routing":{"rules":[rule]}}`；**必须带 `-append`，不带会整表替换**）。
7. ③ 每个入口：没有才先查 `ss -Htln "sport = :<port>"`（已监听 → exit 22），再 `xray api adi …`（内容 `{"inbounds":[ib]}`）。
8. ④ 再列运行态，每个新入口的 inbound/outbound/rule tag 都必须在，且端口在 10×0.2 s 内开始监听（exit 23）。
9. 任何一步失败：`undo()` 按**加入的逆序**对本次加的对象调用 `api rmi / rmrules / rmo`，然后退出（已有对象不碰）。
10. ⑤ 落盘：`B=<配置目录>/onboard-backups; cp -p $CFG $B/<名>.<时间>.<pid>` → `cp cand.json $CFG.onboard-tmp; chmod/chown --reference=$CFG; sync; mv -f` → 再记 `P1/S1/U1` → `==RESULT {pid_before,start_before,uptime_before,pid_after,start_after,uptime_after,added,skipped,sha_after}`。

控制端读回复核（`:354-375`）：落盘 sha == `cand_sha`；对读回配置再跑一次 `append_only`；已有入口的指纹（每个 inbound JSON 的 sha256[..16]）不变；每个新入口三类 tag 在运行态；`restarted = pid/启动时刻前后不同`（只作报告，不报错）。报告含 `xray_reloads=0`、`xray_full_restart`、diff、前后配置 sha。

崩溃恢复：运行态已有而文件没有 → 重跑时跳过 API，只补落盘；两边都有 → 无操作。

### 4.4 合批（"合批窗口"，`engine.rs:415-441`）

- 行到 P13 → 置 `ready_for_relay`，`ready_since` 记第一次就绪时间（`db.rs:311`）。
- 每个调度周期（300 ms）对每台线路机：若该线路机已有批次在执行（`relay_busy`）则等；`coming` = 同线路机仍在 `running/queued` 且步骤早于 P13 的行数（等闸门、等人、失败的不算，不拖别人）；**发车条件** = `coming == 0` 或 `最老就绪 ≥ relay_window_s(30 s)` 或 `就绪数 ≥ relay_batch_max(20)`。每次最多取 `relay_batch_max` 个。因此单台上架没有等待窗口。
- 每批在库里记一条 `relay_batches`（`merging → done|partial|failed|interrupted`），批次号按 (batch, relay) 递增。
- 成员置 `in_relay_batch`，P13 步骤 start。
- **整批失败**且成员 >1：逐台单独 `attach_batch` 以隔离坏节点，好的照常上线（`:232-249`）。
- **端口冲突**（成员 conflict 且消息含「端口」）：作废预留，把 P10/P12/P13 置回 queued，行回到 P10 重新分配（`:260-267`）。其它 conflict → 行 `blocked`；其它错误 → `failed`。
- 成功的成员：P13 `succeeded`，行回到 `running` 的 P14，继续 P14→P15→P16。
- 前端 `relay_groups` 的 `collecting` 状态是窗口内已就绪的行（`view.rs:104-114`）。

### 4.5 摘除（补偿 / 唯一的反向实现）`detach`（`relay.rs:394-435`）

最多 3 次 CAS 重试；snapshot → 候选配置删掉该行的 inbound/outbound/rule → `guard::remove_only`（除这三个对象外逐字节不变）→ 远端 flock + CAS + `xray -test` → **先 `api rmi`（入口）、再 `rmrules`、最后 `rmo`**（入口在时不能先删规则，否则流量走默认 direct；均 `|| true`）→ 备份 → 原子替换配置。超时 60 s。
只在「补偿」里用；**对已上线（active）行不可用**（`engine.rs:552-555`）。

### 4.6 reattach（`engine.rs:505-532`）

线路机配置被别的流程整份重建后（fleet-cutover 从 x-ui.db 重新生成规格，会丢掉本流水线加的入口），`fleetctl onboard reattach <batch>`：对本批所有 `active` 行按库里原参数（同端口/UUID/REALITY 私钥/shortId）重新 `attach_batch`，零重启、分享链接不变。

---

## 5. 机密

| 机密 | 生成 | 存放 |
|---|---|---|
| root 引导密码 | 人提供 | 不入库；路径写进行 spec（JSON，`rows.spec`）。网页提交写 `work_dir/secrets/<batch>-<uuid>.pw` 0600，**永不删除** |
| `ss_password` | 32 字符随机（§P9） | `secrets` 表明文；节点 `/etc/shadowsocks-rust/config.json`（0640 root:fleet-ss）；线路机 Xray 配置与备份里（outbound.password） |
| `client_uuid` | UUID v4 | `secrets`；线路机配置；分享链接 |
| `reality_private_key` / `reality_public_key` | X25519 随机 | `secrets`；私钥在线路机配置与每份备份里；公钥在分享链接 |
| `short_id` | 8 字节随机 hex | `secrets`；线路机配置；分享链接 |
| `share_link` | P16 拼接 | `secrets` |
| 资产 uuid | UUID v4 | `assets.uuid`（身份 id，非机密） |
| nodeagent 令牌 | fleetd-ingest `issue_token(host_id,"onboard")`，`fe1_…` | 节点 `/etc/fleet-edge/token` 0600 fleet-edge；库里只有 sha256 |
| 上架 SSH 密钥 | 运维预先 `ssh-keygen -t ed25519`（`/etc/fleet/onboard/id_ed25519`，0600） | 中控；公钥推到节点 root；线路机用另一把 `relay_ed25519` |
| 主机公钥 | keyscan | `host_keys` 表（所有观察）+ `known_hosts/<row>`（采用的那一把） |

`onboard.db` 里全部机密**明文**，仅靠文件权限 0600（`WIRING.md:131`）；保险库/sealed box 是未做项。`secret_or_insert` 用 `INSERT OR IGNORE`，保证重试/崩溃重跑不换机密（换了会使已发出去的链接失效）。线路机 `onboard-backups` 里每次改动一份含全部入口私钥的拷贝，无轮转。

---

## 6. 状态机

### 6.1 批次（`batches.status`，`db.rs:221-230`）

```
validated --start/run--> running --(全部行 active|cancelled)--> finished
                           |  \--(有行停在 waiting_input/blocked/failed)--> attention
                           |  \--(cancel 整批)--> cancelling --> cancelled
  启动前置不满足（ss 包/agent 包校验失败）--> attention（error 写入 batches.error）
attention --resume--> running
```
`paused` 是视图派生值：`status='running'` 但租约已过期（执行者崩了，`view.rs:116-120`）。`finished_at` 在 finished/cancelled/attention 时设置。

**租约**（`db.rs:254-275`，`lease_ttl_s=20`）：同一批次同时只有一个执行者；`run_batch`/`cancel`/`reattach` 开始时抢租约，别人持有未过期租约 → 失败；心跳每 `ttl/3` 续约；结束释放。崩溃后租约过期，任何人 `resume` 即可接管。`force` 参数存在但没有调用点。

### 6.2 行（`rows.state` × `rows.current_step`，`model.rs:60-80`）

```
queued --> running --(步骤成功)--> running(下一步) ... --> P13: ready_for_relay --> in_relay_batch --> running(P14) ... --> P16 成功: active
   |          |-- WaitInput  --> waiting_input   (input)
   |          |-- Blocked    --> blocked          (identity/conflict，需 --force)
   |          |-- Fail       --> failed           (重试用尽 / provider / bug / relay)
   |-- waiting_gate（金丝雀闸门未开）
任一非 active 状态 --cancel--> cancelled（逆序补偿）
active 终态；cancel 对 active 行拒绝（「走下线流程」——该流程不存在）
```
- `runnable` = queued/running/waiting_gate；`stopped` = waiting_input/blocked/failed/active/cancelled。
- UI 词表：`ready_for_relay` 对外显示为 `in_relay_batch`；步骤格 `compensated` 显示为 `cancelled`（`view.rs:9-21`）。
- 步骤格：queued → running → succeeded|failed|skipped|waiting_input|blocked；补偿后 compensated；取消时未执行的 queued/waiting_input → cancelled。每次 `step_start` 递增 `attempt` 并写 `step_attempts`。
- 步骤顺序即 P1..P16 线性推进（`next_step`）；P13 由批次调度接手。

### 6.3 金丝雀（`db.rs:155`，`engine.rs:386-403`）

`canary=true` 时 CSV 第 1 行（`i==0`）是金丝雀；闸门 = 金丝雀 `active`（`canary_gate=P16`，默认）或（`P12`）金丝雀已越过 P12/就绪/合批中。闸门未开时其它行置 `waiting_gate`；金丝雀停在 stopped 状态（非 active）时，其它行的原因显示「处理后 resume，或 resume --no-canary」。

### 6.4 恢复 / resume（`engine.rs:324-337`）

`run_batch` 启动时：把上次 `merging` 的线路机批次标 `interrupted`；`in_relay_batch/ready_for_relay` 行 → `ready_for_relay`（重新组批，P13 幂等）；`running/waiting_gate` 行 → `queued`（当前步骤从「检查」重跑）；`waiting_input/blocked/failed` 行**保持原状**，需要 `resume --row`（补密码 `--password-file`、DD 闸门确认、重试，blocked 需 `--force`，`engine.rs:476-500`）：写密码文件路径进 spec、`P4` 写 `ack`、步骤置 queued、行置 queued。API 对应 `rows/{name}/actions {provide_input|continue|retry}` + `resume`。

### 6.5 取消（`engine.rs:360-383,535-596`，`api.rs:169-187`）

- 无人执行时 `cancel` 直接抢租约并 `cancel_rows`。
- 有人执行时（租约未过期）只写 `cancel_req`（`"*"` 或行名）；引擎：`"*"` → 停止派发新步骤与新合批，等在途结束后对所有行补偿、批次置 `cancelled`；单行 → 不在执行且不在线路机批次里则立即补偿，否则等。`run_row` 在每步开始前检查取消请求（手上的步骤做完才停）。
- 补偿按步骤**逆序**，只补偿「可撤销」的：`succeeded` 的步骤、`P13` running、`P10` 非 queued、`P11` 执行过（`attempt>0` 且非 skipped/compensated，失败/超时的也补偿，因为节点可能已装、令牌可能已签发）。实现了 undo 的只有 P13、P11、P10、P9、P8、P2（见各步）。**P7 无补偿，P1 的 known_hosts、P3、P12、P15、P16 都不动。** 补偿失败只记 note，不阻塞，行仍置 `cancelled`。

---

## 7. 超时/重试一览

| 步骤 | 超时 | 自动重试 |
|---|---|---|
| P1 | TCP 3×5 s；keyscan 5×(≤15 s)+4×3 s；relay keyscan 20 s；snapshot 40 s | transient/relay：3 次 |
| P2 | 登录探测 20 s；推公钥 40 s | 同上 |
| P3 | 30 s | 同上 |
| P6 | 等 SSH ≤1800 s（轮询 5 s） | provider 不重试 |
| P7 | 360 s（apt 内部 300 s）；密码反测 30 s | transient 重试；sshd/BBR 不符不重试 |
| P8 | 60 s；正/反测各 20 s | transient 重试；探测失败(bug/provider)不重试 |
| P9 | 推二进制 180 s；检查 20 s；脚本 90 s | transient 重试 |
| P10 | snapshot 40 s | relay 重试 |
| P11 | 预检 30 s；准备 30 s；推二进制 180 s；安装 120 s；等第一批 ≤90 s | transient 重试 |
| P12 | 40 s ×(1~2) | transient/relay 重试 |
| P13 | 脚本 120 s（flock 等 60 s）；CAS 最多 3 次 | **不走步骤重试**；失败 → 行 failed，需人工 `retry` |
| P15 | 90 s（内部 curl 3×15 s） | transient 重试 |

---

## 8. 冗余与过度保守（评估）

| 项 | 旧实现 | 评估 |
|---|---|---|
| 公钥采样 | P1 对同一目标连扫 5 次（间隔 3 s，至少 12 s），P6 又 5 次 | 同一视角多次采样对抗不了稳定的冒名者，只能发现抖动。真正有价值的是「第二视角」（所属线路机 keyscan，已有）。建议：中控 1~2 次 + 线路机 1 次，三者一致才钉；P6 同理。无带外指纹，本质仍是 TOFU，要在文档里说清 |
| 出口校验 ×3 | P8 正测（线路机→节点 TCP）、P12（线路机临时 Xray 经 SS 取出口 IP，不等再复测一次）、P15（中控经 VLESS→线路机→节点取出口 IP） | P12 与 P15 的出口 IP 断言重复。保留 **P12 作为改线路机配置前的门**（失败不会污染线路机），保留 **P15 作为接入后的验收**；去掉 P8 正测（P12 覆盖可达性）、P12 的「两次一致才定性」改为通用重试。P8 反测（非白名单被丢弃）有独立价值但每台跑一次太贵：保留在金丝雀行或抽样 |
| 端口检查 ×4 | 分配时 max+1（库+配置）；`attach_batch` 控制端检查配置端口 + `listening` 快照；`append_only` 端口唯一；远端 `listening` 再查一次；落盘后 `listening` 复核 | 以「锁内的远端 `ss` 检查 + DB 唯一约束」为准，其余是同一事实的重复读取 |
| 线路机快照 ×N | plan、P1、P10、attach 前、attach 后各一次，每次一条 SSH + 5 次 xray api | 合批级别只需：plan 一次（展示）、attach 锁内一次、attach 后一次。P1/P10 的快照可去掉（P1 的冲突检查并入 attach 的冲突检查） |
| diff 守卫 | 候选和读回各查一次 + 指纹对比 | 保留「只追加」不变量，重写成一次候选检查 + 一次读回 sha/指纹复核即可 |
| P4/P5/P6 | 三步，P5 恒 skipped，P4 只是闸门 | 合成一个可选的「重装闸门」步骤（有 `dd=manual` 时出现） |
| P10 | 无机器侧动作，只分配端口/生成机密/写资产 | 并入 P13 的前置阶段（合批时分配），去掉独立步骤和一次快照；机密可在 plan 时生成 |
| P14 | **恒 skipped，空步骤** | 删除；若将来 relayd 需要注册探测，作为 P16 的一部分 |
| P16 | 只有库内写入 + 分享链接 | 与 P15 合并为「验收并上线」 |
| P7/P8/P9 | 三个脚本三次 SSH 往返，每个都幂等 | 合成一个幂等的「节点收敛」脚本（基线 + 防火墙 + SS），一次往返，一次输出验证；resume 粒度由幂等保证 |
| P11 | 阻塞线路交付；失败会让线路上线失败；依赖 fleetd 内存令牌后端 | 监测不应阻塞交付：放到上线之后异步执行，独立状态；与 `rollout.sh` 共用同一份安装逻辑，避免「逐项一致」的字节级复制 |
| 前端 `options` | `canary_gate`/`relay_batch_max`/`dry_run` 入了哈希但不生效 | 只放真正生效的选项 |

**其它缺陷/风险（重写时处理）**：
1. 计划哈希不重算（§2）。
2. `node-NN` email 同批内可能冲突（§P10）。
3. 分享链接 fragment 含节点 IP（§P16）。
4. 备份目录无轮转、含私钥；网页提交的密码文件不删除。
5. `apt-get install` 无 `update`；仅 Debian/Ubuntu；无内存/发行版预检。
6. NAT 机的来源 IP 假设未验证。
7. 没有 category 校验（relay 类套餐可被当节点）。
8. P12/P15 临时配置：P12 在线路机经 stdin（含 SS 密码，写 `/dev/shm`），P15 在中控经命令行传 base64。
9. 目录/CSV 到后端目录 schema 不一致（§1.3）。
10. 引导密码用完后「请在供应商面板重置」只是一句提示，没有强制/跟踪。

---

## 9. 重写建议：最小步骤集与「退役」

### 9.1 建议的最小流水线

节点（逐行）：
1. **N1 预检与身份**：TCP + 中控 keyscan（≤2 次）+ 线路机视角 keyscan → 钉公钥 → 采集 facts（arch、os、systemd、nft、MAC、DMI uuid）→ 校验 x86_64、MAC。冲突检查合并进 attach。
2. **N2 引导密钥**（条件：密钥登录失败）：与旧 P2 相同；密码被拒 → 等待输入。
3. **N3 重装闸门**（条件：`dd=manual`）：合并 P4–P6；验收用「公钥变了 + DMI uuid/MAC 一致」。
4. **N4 节点收敛**：一个幂等脚本，包含 P7（sshd 仅密钥 + BBR）、P8（ss_guard nft + 持久化）、P9（ssserver + systemd 单元）；一次输出，逐项断言；仅对金丝雀/抽样跑 nft 反测和密码登录反测。
5. **N5 线路机预检**：在所属线路机上用临时 Xray 经 SS 取出口 IP（旧 P12，单次，通用重试）。
批次（逐线路机，合批窗口 30 s / 20 个 / 同线路机无人在路上即发车）：
6. **R1 分配 + 接入**：分配端口（max+1、永不复用）、email（建议一律 `t<团队>-<NN>` 避免冲突）、生成机密 → `attach_batch`（旧 P13 的全部机制：flock+CAS、`xray -test`、ado→adrules -append→adi、运行态核对、原子落盘、备份、逆序撤销、只追加守卫、整批失败后逐台隔离、端口冲突重分配）。
节点（逐行）：
7. **N6 验收并上线**：中控 VLESS 客户端 E2E（旧 P15）通过 → 资产 `active`、分配 `active`、生成分享链接（不要把节点 IP 放进 fragment）。
旁路：
8. **A1 nodeagent**（异步、不阻塞上线）：旧 P11 的全部机器侧效果，独立状态与重试。

对应关系：旧 P1+P3 → N1，P2 → N2，P4–P6 → N3，P7+P8+P9 → N4，P12 → N5，P10+P13 → R1，P15+P16 → N6，P11 → A1，P14 → 删除。16 步 → 6 个主步骤 + 1 个旁路。

保留的行为约束：只追加 diff 守卫；零重启；flock + sha256 CAS；`xray -test`；端口永不复用；机密 `INSERT OR IGNORE`（重跑不换）；金丝雀先行；租约 + 可 resume；`(ip, ssh_port)` 单飞；blocked 类（identity/conflict）必须人工 `--force`。

### 9.2 缺失的反向操作：退役（retire）

旧系统只能补偿「未上线」的行，且 P7 以及 P1/P3 的副作用从不撤销；对 `active` 行明确拒绝（`engine.rs:552-555`）。重写必须提供 `retire(node)`，按以下**安全顺序**（先切流量，再拆节点，最后清登记，每步幂等，可从中断处继续；任何一步失败不影响已完成的前序步骤）：

0. **前置**：资产 `lifecycle: active → retiring`；确认没有别的批次在操作该线路机（获取与 attach 相同的租约/锁）；记录当前线路机配置 sha 作为审计。
1. **线路机摘除**（停止对外服务，必须第一步）：`detach`——远端 flock + CAS + `xray -test`，依次 `api rmi inbound-<port>`、`api rmrules r-inbound-<port>`、`api rmo <otag>`（顺序不可改），备份后原子写回配置；用 `remove_only` 守卫保证除这三个对象外字节不变；复核：运行态三类 tag 都不在、端口不再监听、配置 sha 已变、Xray pid/启动时刻不变。该端口标记 `retired`（永不复用）。
2. **nodeagent**：先 `REMOVE_SCRIPT`（disable --now、删单元/drop-in/状态目录；非 relayd 机器删 `/opt/fleet-edge`、`/etc/fleet-edge`、用户/组 `fleet-edge`），再撤销该 host_id 全部令牌（顺序不可反）。
3. **SS**：`systemctl disable --now fleet-ssserver`；删 `/etc/systemd/system/fleet-ssserver.service`、`/usr/local/bin/ssserver`、`/etc/shadowsocks-rust/`；`daemon-reload`；（新增）`userdel fleet-ss`。复核 8388 不再监听。
4. **防火墙**：`nft delete table inet ss_guard`；`rm /etc/fleet/nft/ss_guard.nft`；从 `/etc/nftables.conf` 删 include 行。（可选，机器要继续使用时）保留 nftables 包与 `nftables.service`。
5. **机器级基线（可选，`--wipe`）**：`/etc/sysctl.d/90-fleet-bbr.conf`（v1：`99-zz-fleet-tune.conf` 与 journald 的 `90-fleet.conf`，退役清理同样不动；运行态不回退）、`/etc/ssh/sshd_config.d/00-fleet-keyonly.conf` + sshd reload、sshd_config 首行 Include——**默认保留**（机器仍归我方、回退会重新开放密码登录）；机器退还供应商前才由人决定是否清理，因为供应商会销毁/重装。
6. **接入凭据**（最后）：仅当机器要交还/不再管理时，从 `/root/.ssh/authorized_keys` 删除上架公钥行（`grep -vxF`）；**一旦删除即永久失联**，所以放在所有机器侧步骤之后，并且要二次确认。
7. **中控清理**：资产 `lifecycle=retired`（唯一索引自动释放 `(team, number)` 与 `(relay, relay_port)` 的占用）；`allocations` 置 `retired`；删除 `known_hosts/<row>`；删除对应 `.pw` 文件；清除 `secrets`（`ss_password`、`reality_private_key`、`share_link` 等）；保留步骤/事件审计记录与一条「已退役」事件；释放 `rows_live_ip`（行置终态）。
8. **线路机备份**：退役后按保留期清理 `onboard-backups` 中含该入口私钥的旧备份（或用新机制避免备份里存私钥）。

另需补充（旧系统缺失的相关操作）：**更换线路机**（detach 旧 + 更新节点 nft 白名单 + attach 新 + 新分享链接）、**轮换机密**（SS 密码需同时改节点与线路机 outbound，REALITY 密钥改入口并重发链接）、**新线路机开通**（装 fleet-xray、API、配置、`known_hosts` 钉住、`allow_ips`；GoMami 的 relay 类套餐就是这类机器）、以及用 DB 为唯一事实源的**声明式 reconcile**（取代 `reattach`）。
