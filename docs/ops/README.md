# 运维:监控与告警 / Ops: monitoring & alerting (M220)

关闭 [`docs/DEPLOYMENT.md`](../DEPLOYMENT.md) §11 上线前检查清单的「监控接入 `/metrics` + 告警」。
每个节点经 `[metrics] listen` 暴露 Prometheus 文本(配 token 时带 `Authorization: Bearer
<METRICS_TOKEN>`);抓取后加载 [`prometheus-alerts.yml`](prometheus-alerts.yml) 的告警规则。

## 抓取配置(示例)

```yaml
scrape_configs:
  - job_name: zhixing
    authorization: { credentials: "<METRICS_TOKEN>" }   # [metrics] auth_token
    static_configs:
      - targets: ["node21:9121", "node22:9122", "node23:9123", "node24:9124"]
rule_files:
  - prometheus-alerts.yml
```

> 校验规则语法:`promtool check rules docs/ops/prometheus-alerts.yml`。

## 告警一览(每条映射到真实导出指标 + 处置)

| 告警 | 触发 | 含义 / 处置 |
|---|---|---|
| **ZhixingChainHeightStalled** | `zhixing_height` 10m 无增长 | 共识卡住(失去 quorum / 分区 / 提议人全挂)。查 `ZhixingNoPeers`、验证人在线度、各节点日志。`create_empty_blocks=false` 的空闲链可能误报,结合其他告警判断。 |
| **ZhixingValidatorMissingCommits** | 某验证人缺席 >50% liveness 窗口(M212) | 该验证人疑似离线。持续则经验证人集变更移除(无自动 jailing)。本节点 cert 视角,不同节点可略有差异。 |
| **ZhixingEquivocationObserved** | `zhixing_slashing_events_total` 1h 内增长 | 观测到可归因双签并已作为证据广播。排查作恶验证人。 |
| **ZhixingNoPeers** | `zhixing_peers_connected == 0` 持续 2m | 节点孤立(地址错 / 防火墙 / TLS·auth 握手失败)。无法同步或投票。 |
| **ZhixingRpcAuthFailureSpike** | RPC 401 速率升高 | 客户端配置错或凭证猜测。核对 `[rpc] auth_token`。 |
| **ZhixingMetricsAuthFailureSpike** | /metrics 401 速率升高 | 抓取器配置错或探测。核对 `[metrics] auth_token`。 |
| **ZhixingMempoolSaturated** | mempool >90% 容量(有界时) | tx 背压;出块未排空或负载超吞吐。查落块延迟、出块是否停。 |
| **ZhixingSeenTxCacheSaturated** | 去重缓存 >90%(有界时) | flood 压力;淘汰仅致有界重放(非安全问题)。 |
| **ZhixingBlockApplyLatencyHigh** | 落块 p99 > 2s | 磁盘慢(fsync)/ CPU 压力 / 块过大。 |

## 远程签名器(M217–219)观测

远程签名器是独立进程(`node signer`),无自有 `/metrics`。其健康经**节点侧**间接可见:
签名器宕机/拒签 → 该验证人 fail-closed 不投票 → 表现为 `ZhixingValidatorMissingCommits`
(甚至全网 `ZhixingChainHeightStalled`,若失去 quorum)。另查 `node signer` 日志与高水位文件。

## 已知边界

- 指标为**每节点本地视角**(如 `zhixing_validator_missed_commits` 依赖本节点收到的 cert 子集),
  用于观测与运维决策,非共识裁决。
- 阈值为起点,按出块间隔(默认 1s)与集群规模调。
- 无内置 Alertmanager 路由/抑制规则;按运营方既有告警栈接入。
