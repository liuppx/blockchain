IP=$1
if [[ -z "${IP}" ]]; then
  IP=127.0.0.1
fi

GETH_URL=http://${IP}:8545
BEACON_URL=http://${IP}:3500

echo "1. 检查执行层区块状态:"
curl -s -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
      ${GETH_URL}

echo "\n2. 检查最新区块详情:"
curl -s -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["latest",false],"id":1}' \
      ${GETH_URL}


echo "\n3. 检查创世区块:"
curl -s -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["0x0",false],"id":1}' \
      ${GETH_URL}

echo "\n4. 检查同步状态:"
curl -s -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' \
      ${GETH_URL}

echo "\n5. 检查peer连接:"
curl -s -X POST -H "Content-Type: application/json" \
      --data '{"jsonrpc":"2.0","method":"net_peerCount","params":[],"id":1}' \
      ${GETH_URL}

echo "\n1. 检查beacon chain API是否可用:"
curl -s ${BEACON_URL}/eth/v1/node/health

echo "\n2. 检查同步状态:"
curl -s ${BEACON_URL}/eth/v1/node/syncing

echo "\n3. 检查当前slot:"
curl -s ${BEACON_URL}/eth/v1/beacon/headers/head

echo "\n4. 检查验证者状态:"
curl -s "${BEACON_URL}/eth/v1/beacon/states/head/validators"

echo "\n5. 检查最终确定状态:"
curl -s ${BEACON_URL}/eth/v1/beacon/states/head/finality_checkpoints
