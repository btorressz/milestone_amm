// No imports needed: web3, anchor, pg, spl are globally available in Solana Playground
//Still reviewing test file 
/*
describe("Milestone AMM", () => {
  const SEED_MARKET = Buffer.from("market");
  const SEED_POSITION = Buffer.from("position");
  const DECIMALS = 6; // USDC-style
  const ONE = 10 ** DECIMALS;

  let usdcMint: web3.PublicKey;
  let userUsdcAta: web3.PublicKey;

  // Derived deterministically per test run
  const milestoneId = Buffer.from("milestone-1");

  // Helper: airdrop SOL
  const airdrop = async (pk: web3.PublicKey, lamports = 2 * web3.LAMPORTS_PER_SOL) => {
    const sig = await pg.connection.requestAirdrop(pk, lamports);
    await pg.connection.confirmTransaction(sig, "confirmed");
  };

  // Helper: make a mint with 6 decimals and mint tokens to a destination ATA
  const createMintAndMintTo = async (
    mintAuthority: web3.PublicKey,
    freezeAuthority: web3.PublicKey | null,
    toOwner: web3.PublicKey,
    amount: number
  ) => {
    const mintKp = new web3.Keypair();

    // Create mint account
    const rent = await spl.getMinimumBalanceForRentExemptMint(pg.connection);
    const tx = new web3.Transaction().add(
      web3.SystemProgram.createAccount({
        fromPubkey: pg.wallet.publicKey,
        newAccountPubkey: mintKp.publicKey,
        lamports: rent,
        space: spl.MINT_SIZE,
        programId: spl.TOKEN_PROGRAM_ID,
      }),
      spl.createInitializeMintInstruction(
        mintKp.publicKey,
        DECIMALS,
        mintAuthority,
        freezeAuthority,
        spl.TOKEN_PROGRAM_ID
      )
    );

    await web3.sendAndConfirmTransaction(pg.connection, tx, [pg.wallet.payer, mintKp]);

    // Get/create recipient ATA
    const ata = spl.getAssociatedTokenAddressSync(
      mintKp.publicKey,
      toOwner,
      false,
      spl.TOKEN_PROGRAM_ID,
      spl.ASSOCIATED_TOKEN_PROGRAM_ID
    );
    const createAtaIx = spl.createAssociatedTokenAccountInstruction(
      pg.wallet.publicKey,
      ata,
      toOwner,
      mintKp.publicKey,
      spl.TOKEN_PROGRAM_ID,
      spl.ASSOCIATED_TOKEN_PROGRAM_ID
    );
    // Safe: no-op if already exists (Playground often allows sending directly; we can guard)
    try {
      await web3.sendAndConfirmTransaction(
        pg.connection,
        new web3.Transaction().add(createAtaIx),
        [pg.wallet.payer]
      );
    } catch (e) {
      // ignore "already in use"
    }

    // Mint tokens
    const mintIx = spl.createMintToInstruction(
      mintKp.publicKey,
      ata,
      mintAuthority,
      BigInt(amount),
      [],
      spl.TOKEN_PROGRAM_ID
    );
    await web3.sendAndConfirmTransaction(
      pg.connection,
      new web3.Transaction().add(mintIx),
      [pg.wallet.payer]
    );

    return { mint: mintKp.publicKey, ata };
  };

  // Helper: fetch token balance (as number of base units)
  const getTokenBal = async (ata: web3.PublicKey) => {
    const acc = await spl.getAccount(pg.connection, ata);
    return Number(acc.amount);
  };

  it("end-to-end: init → seed_liquidity → buy → sell", async () => {
    // Ensure we have SOL to pay fees
    await airdrop(pg.wallet.publicKey);

    // 1) Create USDC mint and fund the test wallet
    const userInitialUsdc = 1_000 * ONE; // 1,000 USDC
    const created = await createMintAndMintTo(pg.wallet.publicKey, null, pg.wallet.publicKey, userInitialUsdc);
    usdcMint = created.mint;
    userUsdcAta = created.ata;

    // 2) Derive Market PDA and the market's vault ATA (owned by the market PDA)
    const [marketPda, marketBump] = web3.PublicKey.findProgramAddressSync(
      [SEED_MARKET, pg.wallet.publicKey.toBuffer(), milestoneId],
      pg.program.programId
    );
    const vaultUsdcAta = spl.getAssociatedTokenAddressSync(
      usdcMint,
      marketPda,
      true, // owner is a PDA
      spl.TOKEN_PROGRAM_ID,
      spl.ASSOCIATED_TOKEN_PROGRAM_ID
    );

    // 3) init_market
    const now = Math.floor(Date.now() / 1000);
    const params = {
      bFp: new BN(200_000), // b=0.2 in 1e6 fp
      feeBps: 50,           // 0.50%
      deadlineTs: new BN(now + 3600), // +1h
      gracePeriodSecs: new BN(300),   // 5 min
      maxTradeUsdcFp: new BN(200 * ONE), // 200 USDC per trade
      maxPositionSharesFp: new BN(10_000 * ONE), // large cap
      treasury: null,
    };

    // Call init_market (creates market account + vault ATA)
    await pg.program.methods
      .initMarket(params, Array.from(milestoneId))
      .accounts({
        authority: pg.wallet.publicKey,
        usdcMint,
        vaultUsdc: vaultUsdcAta,
        market: marketPda,
        systemProgram: web3.SystemProgram.programId,
        associatedTokenProgram: spl.ASSOCIATED_TOKEN_PROGRAM_ID,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        rent: web3.SYSVAR_RENT_PUBKEY,
      })
      .rpc();

    // 4) seed_liquidity: move some USDC from user → vault
    const seedAmount = 500 * ONE; // 500 USDC
    const seedTx = await pg.program.methods
      .seedLiquidity(new BN(seedAmount))
      .accounts({
        authority: pg.wallet.publicKey,
        market: marketPda,
        authorityUsdc: userUsdcAta,
        vaultUsdc: vaultUsdcAta,
        tokenProgram: spl.TOKEN_PROGRAM_ID,
      })
      .rpc();

    await pg.connection.confirmTransaction(seedTx, "confirmed");

    // Check balances after seeding
    const userAfterSeed = await getTokenBal(userUsdcAta);
    const vaultAfterSeed = await getTokenBal(vaultUsdcAta);
    // user decreased by ~seedAmount; vault increased by seedAmount
    if (!(userInitialUsdc - userAfterSeed >= seedAmount * 0.99)) {
      throw new Error("User USDC did not decrease as expected after seed_liquidity");
    }
    if (!(vaultAfterSeed >= seedAmount)) {
      throw new Error("Vault USDC did not increase as expected after seed_liquidity");
    }

    // 5) buy (side=Hit). This will init the position PDA
    const [positionPda] = web3.PublicKey.findProgramAddressSync(
      [SEED_POSITION, marketPda.toBuffer(), pg.wallet.publicKey.toBuffer()],
      pg.program.programId
    );

    const buyUsdc = 100 * ONE; // spend 100 USDC
    const minShares = 1;       // allow solver to pick size
    const buyTx = await pg.program.methods
      .buy({ hit: {} }, new BN(buyUsdc), new BN(minShares))
      .accounts({
        user: pg.wallet.publicKey,
        market: marketPda,
        userUsdc: userUsdcAta,
        vaultUsdc: vaultUsdcAta,
        position: positionPda,
        treasuryUsdc: vaultUsdcAta, // not used since treasury=null; still pass a writable token account
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .rpc();
    await pg.connection.confirmTransaction(buyTx, "confirmed");

    // Fetch position and assert shares > 0 on HIT side
    const pos = await pg.program.account.position.fetch(positionPda);
    const hitShares = new anchor.BN(pos.hitSharesFp as any); // Anchor returns i128 as BN-like
    if (hitShares.lte(new BN(0))) {
      throw new Error("Expected positive hit_shares_fp after buy");
    }

    // 6) sell half the shares
    const half = hitShares.div(new BN(2));
    const minUsdcOut = new BN(1); // accept anything >0 after fee
    const sellTx = await pg.program.methods
      .sell({ hit: {} }, half, minUsdcOut)
      .accounts({
        user: pg.wallet.publicKey,
        market: marketPda,
        userUsdc: userUsdcAta,
        vaultUsdc: vaultUsdcAta,
        position: positionPda,
        treasuryUsdc: vaultUsdcAta, // again, passed but unused without treasury
        tokenProgram: spl.TOKEN_PROGRAM_ID,
        systemProgram: web3.SystemProgram.programId,
      })
      .rpc();
    await pg.connection.confirmTransaction(sellTx, "confirmed");

    // Position should reduce
    const posAfterSell = await pg.program.account.position.fetch(positionPda);
    const hitAfter = new anchor.BN(posAfterSell.hitSharesFp as any);
    if (!hitAfter.lt(hitShares)) {
      throw new Error("Expected hit_shares_fp to decrease after sell");
    }

    // User USDC should go up relative to after-buy balance
    const userFinal = await getTokenBal(userUsdcAta);
    if (!(userFinal > userAfterSeed - buyUsdc * 0.5)) {
      // Not exact math (fees, LMSR), but balance should recover some
      throw new Error("Expected user USDC to increase after selling shares");
    }

    // Basic market fetch sanity
    const marketAcc = await pg.program.account.market.fetch(marketPda);
    if (!marketAcc) throw new Error("Market account not found");

    console.log("✅ init_market, seed_liquidity, buy, sell — passed");
  });
});
*/