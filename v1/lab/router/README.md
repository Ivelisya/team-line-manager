# 实验室软路由 lab-router1

模拟生产的三台软路由（iStoreOS 24.10 + OpenClash），给「软路由纳入面板」做端到端测试：接入脚本、经隧道采集、出口与客户端统计、告警。
采集脚本看到的东西（uci、进程名、mihomo 接口、配置格式、ubus、/proc）尽量和生产一样；不一样的地方见文末。

| 项 | 值 |
|---|---|
| 位置 | PVE 容器 **170**（非特权 LXC，`ostype: unmanaged`，2 核、512 MB、1 GB 盘，标签 fleetlab，`onboot 0`） |
| 系统 | OpenWrt **24.10.8** x86_64 官方 rootfs（downloads.openwrt.org，按同目录 sha256sums 校验），主机名 lab-router1，时区 Asia/Shanghai |
| 网络 | wan = eth0 静态 **192.168.31.70**/24，网关与 DNS 192.168.31.2（接 PVE 的 vmbr0，就是家里的局域网）；lan = 没有端口的空网桥 br-lan 192.168.124.1/24 |
| 登录 | dropbear 22（root）。fleet-dev 上：`ssh -i /etc/fleet/ssh/id_ed25519 root@192.168.31.70`（中控公钥已在 `/etc/dropbear/authorized_keys`）；PVE 上：`pct enter 170` |
| 密码 | root 密码随机，存 `fleet-dev:/root/lab-root-passwords/lab-router1`（0600）；OpenClash 的 secret（`uci get openclash.config.dashboard_password`，16 位随机）只在路由上。都不打印 |

## 怎么建

```bash
lab/mkrouter.sh     # 本机 Git Bash；经 ssh pve 与 fleet-dev，约 1 分钟
```

1. PVE：从官方 releases 目录下载 rootfs 与 sha256sums、校验后放进 `local:vztmpl`；核对 `/root/lab-router/clash_meta` 的 sha256（生产上拷来的 OpenClash 核心，不从别处下载）；没有容器 170 就建，**第一次启动前**把 `uci/network`、`uci/system` 写进去——OpenWrt 默认把唯一的网口当 lan、开 DHCP 与 RA，eth0 接的是家里的局域网，绝不能让它先用默认配置起来。
2. 路由（`setup.sh`）：删 procd-ujail（见下）、opkg 官方源装 openssh-client / openssh-keygen / curl、加中控公钥（已有不重复加）、设 root 密码、防火墙放行家里局域网来的 22、放模拟 OpenClash / router-tunnel 空壳 / 流量发生器。
3. 出口（`gen-exits.sh`）：在 fleet-dev 上用实验室 admin 会话（同 `lab/e2e.sh`，用完退出登录）取实验室真实线路机（192.168.31.x）上在用入口的分享链接，经管道直接交给路由生成 OpenClash 源配置；链接不落盘、不打印，**节点参数不进仓库**。`claude-lab` 这把 API 密钥没有 secrets 权限，`fleetctl share` 取不了链接。
4. 自检（`check.sh`）：系统、wan、OpenClash 进程与版本、出口、延迟、按出口的连接数、conntrack。

幂等：模板已下载就只校验、容器已有就不重建；其余每次按仓库里的文件修正，内容没变的文件不动；出口每次按中控现状重新生成，没变就不重启核心（重启会断掉所有连接）。只动容器 170 与 PVE 上的 `/root/lab-router`。

## 路由上有什么（对照生产）

| 路径 | 内容 |
|---|---|
| `/etc/config/openclash` | `config openclash 'config'`：enable 1、config_path `/etc/openclash/config/lab-exits.yaml`、cn_port 9090、dashboard_password、en_mode rule（模板 `uci/openclash`；每次按模板重写，只保留已有的 dashboard_password） |
| `/etc/openclash/core/clash_meta` | 生产同款核心 mihomo alpha-g99e68e9（sha256 `76c21f00…be50b`）；`/etc/openclash/clash` 是指向它的符号链接，经它启动，所以 `pidof clash` 与 `pidof clash_meta` 找到同一个进程 |
| `/etc/init.d/openclash` | procd（`openclash.init`）：源配置去掉 external-controller / secret 后复制成运行配置 `/etc/openclash/lab-exits.yaml`（0600），注入 `external-controller: 0.0.0.0:9090` 与 `secret`，再起核心。start / stop / restart / reload；reload 时运行配置变了才重启核心 |
| `/etc/openclash/config/lab-exits.yaml` | 源配置（0600）：生产那种块格式的 proxies、没有代理组（接口里只有内置 GLOBAL）、mixed-port 7890、mode rule、规则 `DOMAIN-SUFFIX,<镜像站>,节点0x` |
| `/etc/init.d/router-tunnel`、`/etc/router-tunnel/ssh_config` | 空壳（`router-tunnel.init`）：命令同生产（`ssh -F … -N router-relay`），但 router-relay 指向文档保留地址 192.0.2.1，**默认不启用**。验证接入脚本不碰它：文件与启用状态前后不变 |
| `/etc/init.d/lab-traffic`、`/usr/share/lab-router/lab-traffic.sh`、`/etc/sysctl.d/90-lab-traffic.conf` | 流量发生器（见下），开机自启 |
| `/usr/share/lab-router/{gen-exits,check}.sh` | 生成出口配置、自检 |
| `/etc/dropbear/authorized_keys` | 中控公钥（fleet-dev 的 `/etc/fleet/ssh/id_ed25519.pub`），0600 |

还有 ucode、jsonfilter、ubus、uci、nft（fw4）、dnsmasq、odhcpd、LuCI（uhttpd 80 / 443，wan 不放行）。

## 出口

`gen-exits.sh` 按「线路机名、端口」排实验室入口，依次命名 节点01…（最多 6 个），再加 **节点99**：server 是 192.0.2.1（连不上），用来测出口故障告警，延迟测试一定失败、不会有连接。server:port 就是「线路机 IP + 入口端口」，面板按这个对上 fleet 入口。

哪个出口对应哪个入口、端口是多少，**以最近一次 `lab/mkrouter.sh` 生成的为准**（第 3 步「出口」打印出口、server:port 与对应的入口；`fleetctl router show lab-router1` 也能看到每个出口对上的入口）。实验室入口一换线路机、轮换凭据、加删节点，端口和对应关系就变，这里不记具体端口。结构上固定的只有：

| 出口 | 是什么 |
|---|---|
| 节点01… | 实验室线路机（192.168.31.x）上的在用入口，按线路机名、端口排序依次命名，最多 6 个；server 是线路机 IP |
| 节点99 | 192.0.2.1（文档保留地址），故意连不上：测出口故障告警（`router_exits_down` 一直开着是正常的） |

实验室入口变了就重跑 `lab/mkrouter.sh`；替换节点、换线路机的作业会自己改路由上的出口，改完不用重跑（重跑也只是按现状重新生成）。

## 流量发生器 lab-traffic

规则把第 i 个出口分给第 i 个镜像站（阿里云、华为云、腾讯云、中科大、南大、火山引擎；清华 tuna / bfsu 对 curl 的 GET 回 403，不用），发生器经 `127.0.0.1:7890` 访问这些域名：

- 每个域名一条慢速长连接：下载 Debian 的 `Packages.xz`（约 9 MB），限速 1 KB/s、最长 10 分钟，断了下一轮重连；
- 每分钟每个域名一次短请求（取 8 KB）。

所以 `/connections` 里每个出口一直有连接与流量（节点99 只有失败的拨号，mihomo 日志里每分钟两条 warning）。来源地址轮流用三个「模拟客户端」192.168.124.101–103（发生器加在 br-lan 上的 /32 地址），连接的 sourceIP 像局域网里的设备。

TCP 收发缓冲上限压到 256 KB（`/etc/sysctl.d/90-lab-traffic.conf`，`lab-traffic.sysctl`）：默认上限下 mihomo 发往 curl 的队列会在长连接开头一下子灌进 4 MB，之后这个出口好几分钟没有流量；压低后开头最多约 0.4 MB，之后稳定 1 KB/s。流量全经实验室线路机与节点走家里的宽带，四个出口一天约 0.5 GB；不用时 `pct exec 170 -- /etc/init.d/lab-traffic stop`（开机仍会自启，要彻底停再 `disable`）。

## 和生产不一样的地方（写采集、看面板时注意）

- **CPU、负载、温度是 PVE 宿主机的**：lxcfs 只虚拟了 /proc/meminfo、/proc/uptime、/proc/cpuinfo 等；容器里 `/proc/stat` 是宿主机两颗 CPU 的累计时间，`/proc/loadavg` 是宿主机负载（编译时常到 5–10），`/sys/class/hwmon` 是宿主机的传感器。内存（MemTotal 512 MB、MemAvailable）与运行时长是容器自己的。busybox `free` 走 sysinfo，显示的是宿主机内存，要读 /proc/meminfo。
- 没有 `/overlay`：根就是 1 GB 的 ext4，存储看 `/`。
- `nf_conntrack_count` 读得到（每个网络命名空间单独计数，fw4 在用 conntrack）；`nf_conntrack_max` 是宿主机的全局值 262144（容器里只读）。
- 内核是 PVE 的 `7.0.14-19-pve`（`uname -r`、`ubus call system board` 的 kernel），board_name 是宿主机 DMI 来的 `lenovo-invalid`；OpenWrt 的 kmod 不会加载。
- 删了 procd-ujail：非特权 LXC 里 ujail 挂载 /dev/log 被拒、建不了 jail，dnsmasq 会一直起不来（本机没有 DNS）。服务改为不进 jail 直接跑。
- 时间跟宿主机走，容器不能改时钟，NTP 关着。
- TCP 收发缓冲上限压到 256 KB（见「流量发生器」），只影响这台路由自己的连接。
- busybox 没有 `nproc`，核数从 /proc/cpuinfo 或 /proc/stat 数。
- **网络形态**：wan 是静态地址、接家里的局域网（生产 team1 / team1b 的 wan 是 dhcp 192.168.1.x、lan 发 DHCP；team2 是旁路由，wan = br-lan）；没有 wan6；lan 是空网桥，没有真实客户端：`/tmp/dhcp.leases` 是空的，邻居表里是 **wan 侧**的机器（网关、fleet-dev、两台线路机），客户端只能从连接的 sourceIP 看到（上面三个模拟客户端，它们是路由自己的地址，不在租约与邻居表里）。
- 防火墙多一条 `Allow-SSH-Lab`（wan 来的 22，限 192.168.31.0/24），方便 fleet-dev 直连；生产 wan 不开 22，经隧道登录。
- router-tunnel 是没启用的空壳（生产在跑）。
- OpenClash 是模拟的：只有运行配置注入与核心；没有 OpenClash 的 LuCI、透明代理（fake-ip / redir / tproxy 的 nft 规则）、DNS 劫持、看门服务；en_mode 是 rule（生产是 fake-ip-mix）。所以连接全从 mixed-port 进来（`metadata.type` 是 HTTPS、`inboundName` 是 DEFAULT-MIXED），生产大多是透明代理进来的。`allow-lan` 开着但 `lan-allowed-ips` 只放 127.0.0.0/8 与 192.168.124.0/24。
- 出口延迟约 700 ms：路径是 线路机 → 节点 → 家里的宽带出口 → 外网，比生产长。

## 常用操作（PVE 上）

```bash
pct exec 170 -- sh /usr/share/lab-router/check.sh                 # 自检
pct exec 170 -- /etc/init.d/openclash stop                        # 模拟「OpenClash 启用但核心没在跑」；start 恢复
pct exec 170 -- sh -c 'uci set openclash.config.enable=0; uci commit openclash; reload_config'   # 模拟没启用（核心随之停掉）；改回 1 再 reload_config 恢复
pct exec 170 -- ifdown wan                                        # 模拟 WAN 断开（fleet-dev 的 SSH 也会断）；ifup wan 恢复
pct exec 170 -- logread -e clash                                  # 核心日志
pct stop 170 && pct destroy 170 --purge                           # 销毁（再跑 mkrouter.sh 重建）
```
