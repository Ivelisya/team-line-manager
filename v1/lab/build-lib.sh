# 编译主机与本机打包的公共函数，由 lab/sync-server.sh、lab/deploy-server.sh、lab/install-fleetctl.sh、
# deploy/central/deploy.sh 等 source（不单独执行）。两种编译方式（docs/OPS.md §9）：
#
#   lab（默认）：家里实验室的 fleet-dev（ssh -F ~/.ssh/fleetlab_config fleet-dev），rustup 装的工具链，源码在 /root/src；
#     fleet-server 编 glibc 版（target/release），fleet-agent、fleetctl 编 x86_64-unknown-linux-musl 静态版。
#   docker：任意一台能 ssh、装了 docker（可以 rootless、不用 sudo、不用装 Rust）的 Linux 主机，如办公网的 dev 机。
#     fleet-server 在 rust:<版本>-slim-trixie 里编 glibc 版（生产中控是 Debian 13；项目没有自定义全局分配器，
#     musl 的 malloc 在多线程服务上慢，所以中控不用 musl），fleet-agent、fleetctl 在 rust:<版本>-alpine 里编 musl 静态版。
#     两个镜像各用自己的 cargo 缓存卷（fleet-v1-cargo / fleet-v1-cargo-gnu）与 target 目录（target / target-gnu），互不覆盖产物。
#
# 配置：环境变量，或本机文件 lab/build.env（不进仓库，见 .gitignore；每行 KEY=value，# 开头是注释，不执行其中的代码）。
# 两处都有时环境变量优先。什么都没配就是 lab，行为与以前一样。
#   FLEET_BUILD=lab|docker
#   FLEET_BUILD_HOST=10.172.17.213          docker 必填：ssh 目标（可写 user@host 或 ~/.ssh/config 里的别名）
#   FLEET_BUILD_DIR=fleet-v1-dev/src        docker：远端源码目录（相对远端家目录，或绝对路径）
#   FLEET_BUILD_DIST=fleet-v1-dev/dist      docker：远端放 xray、ssserver、geoip.dat 的目录；不全时 deploy.sh 不带 dist
#   FLEET_BUILD_MUSL_IMAGE=rust:1.98.1-alpine   FLEET_BUILD_GNU_IMAGE=rust:1.98.1-slim-trixie
#   FLEET_BUILD_MUSL_CACHE=fleet-v1-cargo       FLEET_BUILD_GNU_CACHE=fleet-v1-cargo-gnu
#   FLEET_BUILD_DOCKER_ARGS='--network fleet-v1-dev -e FLEET_TEST_DB=postgres://…'   docker run 的附加参数（跑集成测试要连库时）
# 例：FLEET_BUILD=docker FLEET_BUILD_HOST=10.172.17.213 deploy/central/deploy.sh --dry-run <中控> <域名>

FB_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# lab/build.env：只认 FLEET_BUILD* 这几个键，环境里已经有的不覆盖
if [ -f "$FB_ROOT/lab/build.env" ]; then
  while IFS= read -r _l || [ -n "$_l" ]; do
    case $_l in ''|'#'*) continue ;; esac
    _k=${_l%%=*} _v=${_l#*=}
    _k=${_k#export } _k=${_k// /}
    case $_k in *[!A-Z_]*) continue ;; FLEET_BUILD|FLEET_BUILD_*) ;; *) continue ;; esac
    _v=${_v#[\"\']} _v=${_v%[\"\']}
    [ -n "${!_k+x}" ] || printf -v "$_k" '%s' "$_v"
  done < "$FB_ROOT/lab/build.env"
  unset _l _k _v
fi

FB_MODE=${FLEET_BUILD:-lab}
case $FB_MODE in
  lab)
    FB_DESC=fleet-dev
    FB_DIST_DIR=/opt/fleet/dist ;;
  docker)
    [ -n "${FLEET_BUILD_HOST:-}" ] || { echo "FLEET_BUILD=docker 要配 FLEET_BUILD_HOST（docker 编译主机的 ssh 目标），见 lab/build-lib.sh" >&2; exit 1; }
    FB_DIR=${FLEET_BUILD_DIR:-fleet-v1-dev/src}
    FB_DIST_DIR=${FLEET_BUILD_DIST:-fleet-v1-dev/dist}
    FB_MUSL_IMAGE=${FLEET_BUILD_MUSL_IMAGE:-rust:1.98.1-alpine}
    FB_GNU_IMAGE=${FLEET_BUILD_GNU_IMAGE:-rust:1.98.1-slim-trixie}
    FB_MUSL_CACHE=${FLEET_BUILD_MUSL_CACHE:-fleet-v1-cargo}
    FB_GNU_CACHE=${FLEET_BUILD_GNU_CACHE:-fleet-v1-cargo-gnu}
    FB_DESC="docker@$FLEET_BUILD_HOST:$FB_DIR" ;;
  *) echo "FLEET_BUILD 只能是 lab 或 docker（现在是 ${FB_MODE}）" >&2; exit 1 ;;
esac

# 本机打 tar：macOS 的 bsdtar 默认把扩展属性写成 ._* 文件一起打进去（进了 migrations/ 会让 sqlx::migrate! 编译失败），
# COPYFILE_DISABLE=1 关掉，bsdtar 再加 --no-xattrs；GNU tar（Linux、Windows Git Bash）不认 --no-xattrs，不加
# （macOS 自带的 /bin/bash 是 3.2，set -u 下空数组会报未绑定，这里用字符串）
if tar --version 2>/dev/null | grep -q bsdtar; then FB_TAR_OPT=--no-xattrs; else FB_TAR_OPT=; fi
fb_tar() { COPYFILE_DISABLE=1 tar $FB_TAR_OPT "$@"; }
# 远端解包后的兜底：删当前目录下的 ._*（跳过 target 目录，那里很大）。拼进远端命令里用
FB_RM_APPLEDOUBLE="find . \\( -name target -o -name 'target-*' \\) -prune -o -name '._*' -type f -exec rm -f {} +"

# 在编译主机上执行一条 shell 命令（lab：fleet-dev 的 root；docker：编译主机上的登录用户，家目录下）
fb_ssh() {
  if [ "$FB_MODE" = lab ]; then ssh -F "$HOME/.ssh/fleetlab_config" fleet-dev "$@"
  else ssh "$FLEET_BUILD_HOST" "$@"
  fi
}

# 把 Rust 工作区（Cargo.toml、Cargo.lock、crates、migrations、deploy）同步到编译主机的源码目录
fb_sync() {
  local d=/root/src
  [ "$FB_MODE" = lab ] || d=$FB_DIR
  fb_tar -C "$FB_ROOT" -czf - --exclude=target Cargo.toml Cargo.lock migrations deploy crates \
    | fb_ssh "mkdir -p $d && cd $d && rm -rf crates migrations deploy && tar --no-same-owner -xzf - && $FB_RM_APPLEDOUBLE"
}

# docker：在编译主机的源码目录里起一个编译容器，bash 脚本从标准输入读。$1 = musl | gnu，其余是附加的 docker run 参数（如 -e X=Y）
fb_docker() {
  local kind=$1 img vol tgt pre=
  shift
  case $kind in
    musl) img=$FB_MUSL_IMAGE vol=$FB_MUSL_CACHE tgt=target
          pre='apk add -q musl-dev openssh-client bash coreutils >/dev/null && ' ;;
    gnu)  img=$FB_GNU_IMAGE vol=$FB_GNU_CACHE tgt=target-gnu ;;
  esac
  fb_ssh "cd $FB_DIR && docker run --rm -i $* ${FLEET_BUILD_DOCKER_ARGS:-} -v $vol:/usr/local/cargo/registry -v \$PWD:/src -w /src \
    -e CARGO_TARGET_DIR=/src/$tgt $img sh -c '${pre}exec bash -s'"
}

# 在编译环境的源码目录里执行命令（lab/sync-server.sh 带参数时）：lab 是 fleet-dev 的 /root/src；docker 是 musl 容器（同测试）
fb_run() {
  if [ "$FB_MODE" = lab ]; then fb_ssh "cd /root/src && export PATH=\$HOME/.cargo/bin:\$PATH && $*"
  else printf '%s\n' "$*" | fb_docker musl
  fi
}

# 按已同步的源码编译三个发布版二进制；$1 = 版本号里的 git 短哈希（经 FLEET_GIT_HASH 交给 cargo，版本号成为 0.1.0+<哈希>）
# 成功只打每次 cargo 的最后一行（用时），失败打全部输出：编译错误在前面。
# 脚本经远端 bash 的标准输入送过去，cargo 的标准输入接 /dev/null，免得它（或构建脚本）读走后面的行
fb_build_release() {
  local hash=$1
  local helper='set -euo pipefail
build() { local out; out=$(cargo build "$@" 2>&1 </dev/null) || { printf "%s\n" "$out"; return 1; }; printf "%s\n" "$out" | tail -1; }'
  if [ "$FB_MODE" = lab ]; then
    fb_ssh "FLEET_GIT_HASH=$hash bash -s" <<EOF
$helper
export PATH=\$HOME/.cargo/bin:\$PATH
cd /root/src
build -p fleet-server --release
build -p fleet-agent --release --target x86_64-unknown-linux-musl
build -p fleetctl --release --target x86_64-unknown-linux-musl   # 中控本机的 fleetctl（静态）
EOF
  else
    fb_docker gnu -e FLEET_GIT_HASH="$hash" <<EOF
$helper
build -p fleet-server --release
EOF
    # alpine 镜像的宿主三元组就是 x86_64-unknown-linux-musl，不加 --target：产物在 target/release
    fb_docker musl -e FLEET_GIT_HASH="$hash" <<EOF
$helper
build -p fleet-agent --release
build -p fleetctl --release
EOF
  fi
}

# 只编 fleetctl（musl 静态版），lab/install-fleetctl.sh 用
fb_build_fleetctl() {
  if [ "$FB_MODE" = lab ]; then
    fb_ssh 'bash -s' <<'EOF'
set -euo pipefail
export PATH=$HOME/.cargo/bin:$PATH
cd /root/src
rustup target add x86_64-unknown-linux-musl >/dev/null 2>&1 || true
cargo build -p fleetctl --release --target x86_64-unknown-linux-musl 2>&1 | tail -2
EOF
  else
    echo 'set -o pipefail; cargo build -p fleetctl --release 2>&1 | tail -2' | fb_docker musl
  fi
}

# 把编好的二进制以 tar 流写到标准输出（平铺，不带目录）；参数是要的文件名：fleet-server fleet-agent fleetctl 的任意组合
fb_bins_tar() {
  local srv= musl= f
  for f in "$@"; do if [ "$f" = fleet-server ]; then srv=fleet-server; else musl="$musl $f"; fi; done
  if [ "$FB_MODE" = lab ]; then
    fb_ssh "tar -czf - ${srv:+-C /root/src/target/release fleet-server} ${musl:+-C /root/src/target/x86_64-unknown-linux-musl/release$musl}"
  else
    fb_ssh "cd $FB_DIR && tar -czf - ${srv:+-C \$PWD/target-gnu/release fleet-server} ${musl:+-C \$PWD/target/release$musl}"
  fi
}
