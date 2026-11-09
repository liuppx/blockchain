# 测试网部署 · blockchain.yeying.pub

单机部署:4 个验证节点(n21–n24,只监听 127.0.0.1)+ 区块浏览器 + nginx TLS。

```
Internet ─443─▶ nginx ─┬─ /rpc/ ─▶ 127.0.0.1:9221  (n21 RPC;GET 公开,POST 需 Bearer 令牌)
                       └─ /     ─▶ 127.0.0.1:8080  (浏览器)
```

| 文件 | 作用 |
|---|---|
| `deploy.sh` | 同步并安装;`--genesis` 首次开链(在服务器上生成验证人密钥,销毁旧链) |
| `nginx/blockchain.conf` | 站点配置(TLS 用现有 Let's Encrypt 证书,RPC 限流 10 r/s/IP) |
| `systemd/zhixing-node@.service` | 验证节点(`systemctl status zhixing-node@21`) |
| `systemd/zhixing-explorer.service` | 浏览器 |

## 构建(本机 → linux x86_64)

`ring` 含 C 代码,在 arm64 的 rust 容器里用 x86_64 交叉 gcc 编译(无需模拟):

```sh
docker run --rm -v "$PWD:/src" -v zx-cargo-registry:/usr/local/cargo/registry -w /src/node rust:1.90-bookworm sh -c '
  apt-get update -qq && apt-get install -y -qq gcc-x86-64-linux-gnu libc6-dev-amd64-cross
  rustup target add x86_64-unknown-linux-gnu
  export CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
  cargo build --release --locked --target x86_64-unknown-linux-gnu --target-dir /src/node/target-linux'
```

## 部署

```sh
deploy/deploy.sh --genesis   # 首次:成员名单 testnet/members.toml 必须就绪
deploy/deploy.sh             # 之后:升级程序/脚本,保留链数据
```

## 运维

```sh
journalctl -u zhixing-node@21 -f          # 节点日志
systemctl restart zhixing-node@22         # 重启单个节点(SIGTERM 优雅退出)
cat /opt/zhixing/testnet/out/tokens.env   # RPC 写令牌(只私下发给成员)
```

- 密钥库密码在 `/etc/zhixing/env`(root 600),由 systemd 注入;链数据与密钥在 `/opt/zhixing/testnet/out/`。
- 4 个验证节点在同一台机器上:这是**测试网**的取舍(机器宕机即全链停),不是生产拓扑。
- 旧的 nginx 配置在首次部署时备份到 `/root/blockchain.conf.pre-zhixing.<时间>`。
