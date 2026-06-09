// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "../lib/openzeppelin-contracts/contracts/utils/cryptography/MerkleProof.sol";
import "../lib/sp1-contracts/contracts/src/ISP1Verifier.sol"; // TODO: fix path

contract TinyRollupBridge {
    ISP1Verifier public immutable sp1VerifierGateway;
    bytes32 public immutable rollupProgramVKey;
 
    bytes32 public stateRoot;

    uint256 public depositCount;
    uint256 public processedDepositCount;

    struct ForcedTransaction {
        bytes data;
        uint256 timestamp;
    }
    ForcedTransaction[] public forcedTransactions;
    uint256 public forcedTxCount;
    uint256 public processedForcedTxCount;
    struct Deposit {
        address user;
        uint256 amount;
        uint256 timestamp;
    }
    Deposit[] public pendingDeposits;
    uint256 public constant RECLAIM_WINDOW = 24 hours;
    uint256 public constant FORCED_TX_FEE = 0.001 ether;

    mapping(bytes32 => bool) public nullifiers;

    event StateUpdated(bytes32 indexed oldRoot, bytes32 indexed newRoot);
    event Deposited(address indexed user, uint256 amount, uint256 depositNonce);
    event ForcedTransactionQueued(uint256 indexed nonce, bytes data, address sender);
    event Withdrawn(address indexed user, uint256 amount);

    constructor(
        address sp1VerifierGatewayAddress,
        bytes32 rollupProgramVKey_,
        bytes32 initialStateRoot
    ) {
        sp1VerifierGateway = ISP1Verifier(sp1VerifierGatewayAddress);
        rollupProgramVKey = rollupProgramVKey_;
        stateRoot = initialStateRoot;
    }

    function updateState(
        bytes calldata publicValues,
        bytes calldata proofBytes
    ) public {
        // TODO: require(msg.sender == sequencer, "updateState: unauthorized");

        (bytes32 preStateRoot, bytes32 postStateRoot, bytes32 depositsCommitment, bytes32 forcedTxsCommitment) = abi.decode(publicValues, (bytes32, bytes32, bytes32, bytes32));

        require(preStateRoot == stateRoot, "updateState: invalid pre-state root");

        (uint32 depositsInBatch, bytes28 depositsHash) = decodeCommitment(depositsCommitment);
        (uint32 forcedTxsInBatch, bytes28 forcedTxsHash) = decodeCommitment(forcedTxsCommitment);

        require(truncate(calculateDepositHash(processedDepositCount, depositsInBatch)) == depositsHash, "updateState: deposit data mismatch");
        require(truncate(calculateForcedTxHash(processedForcedTxCount, forcedTxsInBatch)) == forcedTxsHash, "updateState: forced tx data mismatch");

        sp1VerifierGateway.verifyProof(rollupProgramVKey, publicValues, proofBytes);

        processedDepositCount += depositsInBatch;
        processedForcedTxCount += forcedTxsInBatch;
 
        bytes32 oldRoot = stateRoot;
        stateRoot = postStateRoot;

        emit StateUpdated(oldRoot, postStateRoot);
    }

    function deposit() external payable {
        require(msg.value > 0, "deposit: zero deposit");
        pendingDeposits.push(Deposit({
            user: msg.sender,
            amount: msg.value,
            timestamp: block.timestamp
        }));
        depositCount = pendingDeposits.length;
        emit Deposited(msg.sender, msg.value, depositCount);
    }

    function forceTransaction(bytes calldata l2TxData) external payable {
        require(msg.value >= FORCED_TX_FEE, "forceTransaction: not enough fee");
        forcedTransactions.push(ForcedTransaction({
            data: l2TxData,
            timestamp: block.timestamp
        }));
        forcedTxCount = forcedTransactions.length;
        emit ForcedTransactionQueued(forcedTxCount, l2TxData, msg.sender);
    }

    function getDeposits(uint256 start, uint256 count) external view returns (Deposit[] memory) {
        require(start + count <= depositCount, "getDeposits: invalid range");
        Deposit[] memory deposits = new Deposit[](count);
        Deposit[] storage pending = pendingDeposits;
        for (uint256 i = 0; i < count; i++) {
            deposits[i] = pending[start + i];
        }
        return deposits;
    }

    function getForcedTransactions(uint256 start, uint256 count) external view returns (ForcedTransaction[] memory) {
        require(start + count <= forcedTxCount, "getForcedTransactions: invalid range");
        ForcedTransaction[] memory txs = new ForcedTransaction[](count);
        ForcedTransaction[] storage forced = forcedTransactions;
        for (uint256 i = 0; i < count; i++) {
            txs[i] = forced[start + i];
        }
        return txs;
    }

    function calculateDepositHash(uint256 start, uint256 count) public view returns (bytes32) {
        if (count == 0) {
            return keccak256(abi.encodePacked(""));
        }
        require(start + count <= depositCount, "calculateDepositHash: invalid range");
        
        bytes32[] memory hashes = new bytes32[](count);
        Deposit[] storage pending = pendingDeposits;
        for (uint256 i = 0; i < count; i++) {
            hashes[i] = keccak256(abi.encode(pending[start + i]));
        }
        return keccak256(abi.encodePacked(hashes));
    }

    function calculateForcedTxHash(uint256 start, uint256 count) public view returns (bytes32) {
        if (count == 0) {
            return keccak256(abi.encodePacked(""));
        }
        require(start + count <= forcedTxCount, "calculateForcedTxHash: invalid range");

        bytes32[] memory hashes = new bytes32[](count);
        ForcedTransaction[] storage forced = forcedTransactions;
        for (uint256 i = 0; i < count; i++) {
            hashes[i] = keccak256(abi.encode(forced[start + i]));
        }
        return keccak256(abi.encodePacked(hashes));
    }

    function decodeCommitment(bytes32 commitment) internal pure returns (uint32 count, bytes28 hash) {
        assembly {
            count := and(commitment, 0x00000000000000000000000000000000000000000000000000000000FFFFFFFF)
            hash := and(commitment, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000)
        }
    }

    function truncate(bytes32 fullHash) internal pure returns (bytes28 truncated) {
        assembly { truncated := and(fullHash, 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF00000000) }
    }

    function reclaimDeposit(uint256 depositNonce) external {
        revert("reclaimDeposit is not supported ");
    }

    function withdraw(uint256 amount, uint64 nonce, bytes32[] calldata merkleProof) external {
        bytes32 nullifier = keccak256(abi.encodePacked(msg.sender, amount, nonce));
        require(!nullifiers[nullifier], "withdraw: already claimed");

        bytes32 leaf = keccak256(abi.encodePacked(msg.sender, amount, nonce));
        bool isValid = MerkleProof.verify(merkleProof, stateRoot, leaf);
        require(isValid, "withdraw: invalid proof");

        nullifiers[nullifier] = true;
        (bool success, ) = msg.sender.call{value: amount}("");
        require(success, "withdraw: transfer failed");

        emit Withdrawn(msg.sender, amount);
    }
}