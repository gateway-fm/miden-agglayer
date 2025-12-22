#!/bin/bash

args=(
	# L1 RPC node
	--rpc-url http://$(kurtosis port print cdk el-1-geth-lighthouse rpc)
	--insecure
	--value 120
	# default pre-funded dev account
	--private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
	--bridge-address 0x78908F7A87d589fdB46bdd5EfE7892C5aD6001b6
	# L2 rollup chain ID
	--destination-network 1001
	--legacy=false
	# --dry-run
    # --token-address string # address of ERC20 token to use (default is zero)
)

polycli ulxly bridge asset "${args[@]}"
