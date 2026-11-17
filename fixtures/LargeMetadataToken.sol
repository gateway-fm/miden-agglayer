// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// E2E fixture: a real ERC-20 whose large name is generated, not kept in storage.
/// A deterministic nonzero pattern detects truncation, zero-filling and offsets.
contract LargeMetadataToken {
    uint256 public immutable nameLength;
    uint256 public constant totalSupply = 1_000_000 ether;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    constructor(uint256 length) {
        nameLength = length;
        balanceOf[msg.sender] = totalSupply;
        emit Transfer(address(0), msg.sender, totalSupply);
    }

    function name() external view returns (string memory) {
        uint256 length = nameLength;
        bytes memory value = new bytes(length + 32);
        assembly ("memory-safe") {
            mstore(value, length)
            let data := add(value, 32)
            for { let offset := 0 } lt(offset, length) { offset := add(offset, 32) } {
                // ASCII "0123456789ABCDEFGHIJKLMNOPQRSTUV" (32 bytes).
                mstore(add(data, offset), 0x303132333435363738394142434445464748494a4b4c4d4e4f50515253545556)
            }
            // Canonical zero padding; extra allocation keeps this write in bounds.
            mstore(add(data, length), 0)
        }
        return string(value);
    }

    function symbol() external pure returns (string memory) { return "HUGE"; }
    function decimals() external pure returns (uint8) { return 18; }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 permitted = allowance[from][msg.sender];
        if (permitted != type(uint256).max) allowance[from][msg.sender] = permitted - amount;
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) private {
        require(to != address(0));
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }
}
