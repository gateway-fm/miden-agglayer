#!/bin/bash

set -e
set -o pipefail

args=(
	# L1 RPC node
	--rpc-url http://$(kurtosis port print miden-cdk el-1-geth-lighthouse rpc)
	--insecure
	# amount in wei
	--value 1000000000000000000
	# default pre-funded dev account
	--private-key 0x12d7de8621a77640c9241b2595ba78ce443d05e94090365ab3bb5e19df82c625
	--bridge-address 0xC8cbEBf950B9Df44d987c8619f092beA980fF038
	# L2 rollup chain ID
	--destination-network 2
	# beneficiary
	--destination-address 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
	--legacy=false
	# --dry-run
	# address of ERC20 token to use (default is zero)
	# --token-address string
)

polycliLog=$(mktemp)
polycli ulxly bridge asset "${args[@]}" 2>&1 | tee "$polycliLog"

depositCount=$(cat "$polycliLog" | sed 's/\x1b\[[0-9;]*m//g' | grep depositCount | cut -d '=' -f 2)
rm "$polycliLog"

sql="SELECT ready_for_claim FROM sync.deposit WHERE deposit_cnt = $depositCount"
postgres=$(docker ps -q --filter 'name=postgres-001')
while [[ $(docker exec $postgres psql -U bridge_user -d bridge_db -At -c "$sql") != 't' ]]
do
    echo "$(date +%H:%M:%S) waiting ready_for_claim..."
    sleep 5
done

echo "OK"
