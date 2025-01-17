import chai from "chai";
import {ethers} from "hardhat";
import {solidity} from "ethereum-waffle";
import {TestLogicContract} from "../typechain/TestLogicContract";
import {SimpleLogicBatchMiddleware} from "../typechain/SimpleLogicBatchMiddleware";

import {deployContracts, sortValidators} from "../test-utils";
import {examplePowers, getSignerAddresses, signHash, ZeroAddress} from "../test-utils/pure";
import {MintedForDeployer} from "./deployERC20";

chai.use(solidity);
const {expect} = chai;


async function runTest(opts: {
    // Issues with the tx batch
    invalidationNonceNotHigher?: boolean;
    malformedTxBatch?: boolean;

    // Issues with the current valset and signatures
    nonMatchingCurrentValset?: boolean;
    badValidatorSig?: boolean;
    zeroedValidatorSig?: boolean;
    notEnoughPower?: boolean;
    barelyEnoughPower?: boolean;
    malformedCurrentValset?: boolean;
    timedOut?: boolean;
}) {


    // Prep and deploy contract
    // ========================
    const signers = await ethers.getSigners();
    const gravityId = ethers.utils.formatBytes32String("foo");
    // This is the power distribution on the Cosmos hub as of 7/14/2020
    let powers = examplePowers();
    let validators = sortValidators(signers.slice(0, powers.length));

    const {
        gravity,
        testERC20,
        checkpoint: deployCheckpoint
    } = await deployContracts(gravityId, validators, powers);

    // First we deploy the logic batch middleware contract. This makes it easy to call a logic
    // contract a bunch of times in a batch.
    const SimpleLogicBatchMiddleware = await ethers.getContractFactory("SimpleLogicBatchMiddleware");
    const logicBatch = (await SimpleLogicBatchMiddleware.deploy()) as SimpleLogicBatchMiddleware;
    // We set the ownership to gravity so that nobody else can call it.
    await logicBatch.transferOwnership(gravity.address);

    // Then we deploy the actual logic contract.
    const TestLogicContract = await ethers.getContractFactory("TestLogicContract");
    const logicContract = (await TestLogicContract.deploy(testERC20.address)) as TestLogicContract;
    // We set its owner to the batch contract.
    await logicContract.transferOwnership(logicBatch.address);


    // Transfer out to Cosmos, locking coins
    // =====================================
    await testERC20.functions.approve(gravity.address, 1000);
    await gravity.functions.sendToCosmos(
        testERC20.address,
        ethers.utils.formatBytes32String("myCosmosAddress"),
        1000
    );


    // Prepare batch
    // ===============================
    // This code prepares the batch of transactions by encoding the arguments to the logicContract.
    // This batch contains 10 transactions which each:
    // - Transfer 5 coins to the logic contract
    // - Call transferTokens on the logic contract, transferring 2+2 coins to signer 20
    //
    // After the batch runs, signer 20 should have 40 coins, Gravity should have 940 coins,
    // and the logic contract should have 10 coins
    const numTxs = 10;
    const txPayloads = new Array(numTxs);

    const txAmounts = new Array(numTxs);
    for (let i = 0; i < numTxs; i++) {
        txAmounts[i] = 5;
        txPayloads[i] = logicContract.interface.encodeFunctionData("transferTokens", [await signers[20].getAddress(), 2, 2])
    }

    let invalidationNonce = 1
    if (opts.invalidationNonceNotHigher) {
        invalidationNonce = 0
    }

    let timeOut = 4766922941000
    if (opts.timedOut) {
        timeOut = 0
    }


    // Call method
    // ===========
    // We have to give the logicBatch contract 5 coins for each tx, since it will transfer that
    // much to the logic contract.
    // We give msg.sender 1 coin in fees for each tx.
    const methodName = ethers.utils.formatBytes32String(
        "logicCall"
    );

    let logicCallArgs = {
        transferAmounts: [numTxs * 5], // transferAmounts
        transferTokenContracts: [testERC20.address], // transferTokenContracts
        feeAmounts: [numTxs], // feeAmounts
        feeTokenContracts: [testERC20.address], // feeTokenContracts
        logicContractAddress: logicBatch.address, // logicContractAddress
        payload: logicBatch.interface.encodeFunctionData("logicBatch", [txAmounts, txPayloads, logicContract.address, testERC20.address]), // payloads
        timeOut,
        invalidationId: ethers.utils.hexZeroPad(testERC20.address, 32), // invalidationId
        invalidationNonce: invalidationNonce // invalidationNonce
    }


    const digest = ethers.utils.keccak256(ethers.utils.defaultAbiCoder.encode(
        [
            "bytes32", // gravityId
            "bytes32", // methodName
            "uint256[]", // transferAmounts
            "address[]", // transferTokenContracts
            "uint256[]", // feeAmounts
            "address[]", // feeTokenContracts
            "address", // logicContractAddress
            "bytes", // payload
            "uint256", // timeOut
            "bytes32", // invalidationId
            "uint256" // invalidationNonce
        ],
        [
            gravityId,
            methodName,
            logicCallArgs.transferAmounts,
            logicCallArgs.transferTokenContracts,
            logicCallArgs.feeAmounts,
            logicCallArgs.feeTokenContracts,
            logicCallArgs.logicContractAddress,
            logicCallArgs.payload,
            logicCallArgs.timeOut,
            logicCallArgs.invalidationId,
            logicCallArgs.invalidationNonce
        ]
    ));

    const sigs = await signHash(validators, digest);

    let currentValsetNonce = 0;
    if (opts.nonMatchingCurrentValset) {
        // Wrong nonce
        currentValsetNonce = 420;
    }
    if (opts.malformedCurrentValset) {
        // Remove one of the powers to make the length not match
        powers.pop();
    }
    if (opts.badValidatorSig) {
        // Switch the first sig for the second sig to screw things up
        sigs[1].v = sigs[0].v;
        sigs[1].r = sigs[0].r;
        sigs[1].s = sigs[0].s;
    }
    if (opts.zeroedValidatorSig) {
        // Switch the first sig for the second sig to screw things up
        sigs[1].v = sigs[0].v;
        sigs[1].r = sigs[0].r;
        sigs[1].s = sigs[0].s;
        // Then zero it out to skip evaluation
        sigs[1].v = 0;
    }
    if (opts.notEnoughPower) {
        // zero out enough signatures that we dip below the threshold
        sigs[1].v = 0;
        sigs[2].v = 0;
        sigs[3].v = 0;
        sigs[5].v = 0;
        sigs[6].v = 0;
        sigs[7].v = 0;
        sigs[9].v = 0;
        sigs[11].v = 0;
        sigs[13].v = 0;
    }
    if (opts.barelyEnoughPower) {
        // Stay just above the threshold
        sigs[1].v = 0;
        sigs[2].v = 0;
        sigs[3].v = 0;
        sigs[5].v = 0;
        sigs[6].v = 0;
        sigs[7].v = 0;
        sigs[9].v = 0;
        sigs[11].v = 0;
    }

    let valset = {
        validators: await getSignerAddresses(validators),
        powers,
        valsetNonce: currentValsetNonce,
        rewardAmount: 0,
        rewardToken: ZeroAddress
    }

    let logicCallSubmitResult = await gravity.submitLogicCall(
        valset,

        sigs,
        logicCallArgs
    );


    // check that the relayer was paid
    expect(
        await (
            await testERC20.functions.balanceOf(await logicCallSubmitResult.from)
        )[0].toBigInt()
    ).to.equal(MintedForDeployer + BigInt(9010));

    expect(
        (await testERC20.functions.balanceOf(await signers[20].getAddress()))[0].toBigInt()
    ).to.equal(BigInt(40));

    expect(
        (await testERC20.functions.balanceOf(gravity.address))[0].toBigInt()
    ).to.equal(BigInt(940));

    expect(
        (await testERC20.functions.balanceOf(logicContract.address))[0].toBigInt()
    ).to.equal(BigInt(10));

    expect(
        (await testERC20.functions.balanceOf(await signers[0].getAddress()))[0].toBigInt()
    ).to.equal(MintedForDeployer + BigInt(9010));
}

describe("submitLogicCall tests", function () {
    it("throws on malformed current valset", async function () {
        await expect(runTest({malformedCurrentValset: true})).to.be.revertedWith(
            "MalformedCurrentValidatorSet()"
        );
    });

    it("throws on invalidation nonce not incremented", async function () {
        await expect(runTest({invalidationNonceNotHigher: true})).to.be.revertedWith(
            "InvalidLogicCallNonce(0, 0)"
        );
    });

    it("throws on non matching checkpoint for current valset", async function () {
        await expect(
            runTest({nonMatchingCurrentValset: true})
        ).to.be.revertedWith(
            "IncorrectCheckpoint()"
        );
    });


    it("throws on bad validator sig", async function () {
        await expect(runTest({badValidatorSig: true})).to.be.revertedWith(
            "InvalidSignature()"
        );
    });

    it("allows zeroed sig", async function () {
        await runTest({zeroedValidatorSig: true});
    });

    it("throws on not enough signatures", async function () {
        await expect(runTest({notEnoughPower: true})).to.be.revertedWith(
            "InsufficientPower(2807621889, 2863311530)"
        );
    });

    it("does not throw on barely enough signatures", async function () {
        await runTest({barelyEnoughPower: true});
    });

    it("throws on timeout", async function () {
        await expect(runTest({timedOut: true})).to.be.revertedWith(
            "LogicCallTimedOut()"
        );
    });

});

// This test produces a hash for the contract which should match what is being used in the Go unit tests. It's here for
// the use of anyone updating the Go tests.
describe("logicCall Go test hash", function () {
    it("produces good hash", async function () {


        // Prep and deploy contract
        // ========================
        const signers = await ethers.getSigners();
        const gravityId = ethers.utils.formatBytes32String("foo");
        const powers = [2934678416];
        const validators = signers.slice(0, powers.length);
        const {
            gravity,
            testERC20,
            checkpoint: deployCheckpoint
        } = await deployContracts(gravityId, validators, powers);


        // Transfer out to Cosmos, locking coins
        // =====================================
        await testERC20.functions.approve(gravity.address, 1000);
        await gravity.functions.sendToCosmos(
            testERC20.address,
            ethers.utils.formatBytes32String("myCosmosAddress"),
            1000
        );


        // Call method
        // ===========
        const methodName = ethers.utils.formatBytes32String(
            "logicCall"
        );
        const numTxs = 10;

        let invalidationNonce = 1

        let timeOut = 4766922941000

        let logicCallArgs = {
            transferAmounts: [1], // transferAmounts
            transferTokenContracts: [testERC20.address], // transferTokenContracts
            feeAmounts: [1], // feeAmounts
            feeTokenContracts: [testERC20.address], // feeTokenContracts
            logicContractAddress: "0x17c1736CcF692F653c433d7aa2aB45148C016F68", // logicContractAddress
            payload: ethers.utils.formatBytes32String("testingPayload"), // payloads
            timeOut,
            invalidationId: ethers.utils.formatBytes32String("invalidationId"), // invalidationId
            invalidationNonce: invalidationNonce // invalidationNonce
        }


        const abiEncodedLogicCall = ethers.utils.defaultAbiCoder.encode(
            [
                "bytes32", // gravityId
                "bytes32", // methodName
                "uint256[]", // transferAmounts
                "address[]", // transferTokenContracts
                "uint256[]", // feeAmounts
                "address[]", // feeTokenContracts
                "address", // logicContractAddress
                "bytes", // payload
                "uint256", // timeOut
                "bytes32", // invalidationId
                "uint256" // invalidationNonce
            ],
            [
                gravityId,
                methodName,
                logicCallArgs.transferAmounts,
                logicCallArgs.transferTokenContracts,
                logicCallArgs.feeAmounts,
                logicCallArgs.feeTokenContracts,
                logicCallArgs.logicContractAddress,
                logicCallArgs.payload,
                logicCallArgs.timeOut,
                logicCallArgs.invalidationId,
                logicCallArgs.invalidationNonce
            ]
        );
        const logicCallDigest = ethers.utils.keccak256(abiEncodedLogicCall);


        const sigs = await signHash(validators, logicCallDigest);
        const currentValsetNonce = 0;

        // TODO construct the easiest possible delegate contract that will
        // actually execute, existing ones are too large to bother with for basic
        // signature testing

        let valset = {
            validators: await getSignerAddresses(validators),
            powers,
            valsetNonce: currentValsetNonce,
            rewardAmount: 0,
            rewardToken: ZeroAddress
        }

        var res = await gravity.populateTransaction.submitLogicCall(
            valset,

            sigs,

            logicCallArgs
        )

        console.log("elements in logic call digest:", {
            "gravityId": gravityId,
            "logicMethodName": methodName,
            "transferAmounts": logicCallArgs.transferAmounts,
            "transferTokenContracts": logicCallArgs.transferTokenContracts,
            "feeAmounts": logicCallArgs.feeAmounts,
            "feeTokenContracts": logicCallArgs.feeTokenContracts,
            "logicContractAddress": logicCallArgs.logicContractAddress,
            "payload": logicCallArgs.payload,
            "timeout": logicCallArgs.timeOut,
            "invalidationId": logicCallArgs.invalidationId,
            "invalidationNonce": logicCallArgs.invalidationNonce
        })
        console.log("abiEncodedCall:", abiEncodedLogicCall)
        console.log("callDigest:", logicCallDigest)

        console.log("elements in logic call function call:", {
            "currentValidators": await getSignerAddresses(validators),
            "currentPowers": powers,
            "currentValsetNonce": currentValsetNonce,
            "sigs": sigs,
        })
        console.log("Function call bytes:", res.data)

    })
});