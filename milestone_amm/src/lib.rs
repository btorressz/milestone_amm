use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, Transfer},
};

declare_id!("EWN15wnvd4xHtN9j32zDVQm8zyb2nwy1JHXxaPvFimVM");

/// ========== Config ==========
const FP_SCALER: i128 = 1_000_000; // 1e6 fixed point
const MAX_BISECT_ITERS: usize = 60;
const PRICE_MILLI_SCALER: i64 = 1_000;
const SEED_MARKET: &[u8] = b"market";
const SEED_POSITION: &[u8] = b"position";

#[program]
pub mod milestone_amm {
    use super::*;

    /// Initialize a market and its vault ATA (owned by the market PDA).
    pub fn init_market(
        ctx: Context<InitMarket>,
        params: InitParams,
        milestone_id: Vec<u8>,
    ) -> Result<()> {
        require!(params.b_fp >= 10_000 && params.b_fp <= 1_000_000_000_000, AmmError::InvalidB);
        require!(params.fee_bps <= 10_000, AmmError::InvalidFee);
        require!(params.deadline_ts > Clock::get()?.unix_timestamp, AmmError::AfterDeadline);

        let m = &mut ctx.accounts.market;
        m.authority = ctx.accounts.authority.key();
        m.bump = ctx.bumps.market; // Anchor ≥ 0.28
        m.usdc_mint = ctx.accounts.usdc_mint.key();
        m.vault_usdc = ctx.accounts.vault_usdc.key();
        m.b_fp = params.b_fp as i128;
        m.fee_bps = params.fee_bps;
        m.deadline_ts = params.deadline_ts;
        m.grace_period_secs = params.grace_period_secs;
        m.outcome = Outcome::Unresolved;
        m.q_hit_fp = 0;
        m.q_miss_fp = 0;
        m.paused = false;
        m.max_trade_usdc_fp = params.max_trade_usdc_fp as i128;
        m.max_position_shares_fp = params.max_position_shares_fp as i128;
        m.treasury = params.treasury;
        m.milestone_id = milestone_id;
        m.liquidity_usdc_fp = 0;
        m.oracle_signer = None;

        emit!(MarketInitialized {
            market: m.key(),
            b_fp: m.b_fp as i64,
            fee_bps: m.fee_bps,
            deadline_ts: m.deadline_ts,
        });
        Ok(())
    }

    /// Deposit USDC from authority into the market vault (optional but useful).
    pub fn seed_liquidity(ctx: Context<SeedLiquidity>, usdc_amount_fp: u64) -> Result<()> {
        // Only read what we need; we will mutate after CPI.
        let paused = ctx.accounts.market.paused;
        let market_auth = ctx.accounts.market.authority;
        require!(ctx.accounts.authority.key() == market_auth, AmmError::Unauthorized);
        require!(!paused, AmmError::Paused);

        // Transfer USDC from authority ATA to vault
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.authority_usdc.to_account_info(),
                    to: ctx.accounts.vault_usdc.to_account_info(),
                    authority: ctx.accounts.authority.to_account_info(),
                },
            ),
            usdc_amount_fp,
        )?;

        // Now mutate market
        let m = &mut ctx.accounts.market;
        m.liquidity_usdc_fp = m
            .liquidity_usdc_fp
            .checked_add(usdc_amount_fp as i128)
            .ok_or(AmmError::MathOverflow)?;
        Ok(())
    }

    /// Buy virtual shares on one side (HIT or MISS), paying USDC in.
    pub fn buy(
        ctx: Context<Trade>,
        side: Side,
        usdc_in_fp: u64,
        min_shares_out_fp: u64,
    ) -> Result<()> {
        // Read-only snapshot of market fields we'll need for checks/math
        let clock = Clock::get()?;
        let market_key = ctx.accounts.market.key();
        let paused = ctx.accounts.market.paused;
        let outcome = ctx.accounts.market.outcome;
        let deadline_ts = ctx.accounts.market.deadline_ts;
        let max_trade_usdc_fp = ctx.accounts.market.max_trade_usdc_fp;
        let max_pos_fp = ctx.accounts.market.max_position_shares_fp;
        let usdc_mint = ctx.accounts.market.usdc_mint;
        let vault_usdc_pk = ctx.accounts.market.vault_usdc;
        let b_fp = ctx.accounts.market.b_fp;
        let fee_bps = ctx.accounts.market.fee_bps;
        let q_hit0 = ctx.accounts.market.q_hit_fp;
        let q_miss0 = ctx.accounts.market.q_miss_fp;
        let pda_authority = ctx.accounts.market.authority;
        let bump = ctx.accounts.market.bump;
        let milestone_id = ctx.accounts.market.milestone_id.clone();
        let treasury_opt = ctx.accounts.market.treasury;

        // Checks using the snapshot
        require!(!paused, AmmError::Paused);
        require!(outcome == Outcome::Unresolved, AmmError::AlreadySettled);
        require!(clock.unix_timestamp < deadline_ts, AmmError::AfterDeadline);
        require!((usdc_in_fp as i128) <= max_trade_usdc_fp, AmmError::TradeTooLarge);
        require!(ctx.accounts.user_usdc.owner == ctx.accounts.user.key(), AmmError::InvalidOwner);
        require!(ctx.accounts.user_usdc.mint == usdc_mint, AmmError::WrongMint);
        require!(ctx.accounts.vault_usdc.key() == vault_usdc_pk, AmmError::WrongVault);

        // Collect USDC from user to vault first (safer accounting)
        if usdc_in_fp > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.user_usdc.to_account_info(),
                        to: ctx.accounts.vault_usdc.to_account_info(),
                        authority: ctx.accounts.user.to_account_info(),
                    },
                ),
                usdc_in_fp,
            )?;
        }

        // Init or validate position; we only need mutable borrow of position.
        let pos = &mut ctx.accounts.position;
        if pos.owner == Pubkey::default() {
            pos.owner = ctx.accounts.user.key();
            pos.market = market_key;
            pos.hit_shares_fp = 0;
            pos.miss_shares_fp = 0;
        } else {
            require!(pos.owner == ctx.accounts.user.key(), AmmError::Unauthorized);
            require!(pos.market == market_key, AmmError::WrongMarket);
        }

        // Net spendable estimate after fee
        let fee_mul = 10_000u64
            .checked_add(fee_bps as u64)
            .ok_or(AmmError::MathOverflow)?;
        let usdc_in_net_fp_est = (usdc_in_fp as u128)
            .checked_mul(10_000)
            .ok_or(AmmError::MathOverflow)?
            / (fee_mul as u128);

        // Solve delta_q with snapshot values
        let delta_q = solve_delta_q_bisect(
            b_fp,
            q_hit0,
            q_miss0,
            side,
            usdc_in_net_fp_est as i128,
            max_pos_fp,
            position_side_shares(pos, side),
        )?;
        require!(delta_q >= 0, AmmError::MathOverflow);
        require!((delta_q as u64) >= min_shares_out_fp, AmmError::Slippage);

        // Cost and fee on snapshot curve
        let dcost_fp = lmsr_delta_cost(b_fp, q_hit0, q_miss0, side, delta_q)?;
        let fee_fp = (dcost_fp as i128)
            .checked_mul(fee_bps as i128)
            .ok_or(AmmError::MathOverflow)?
            / 10_000i128;
        let total_due_fp = (dcost_fp as i128)
            .checked_add(fee_fp)
            .ok_or(AmmError::MathOverflow)?;
        require!((usdc_in_fp as i128) >= total_due_fp, AmmError::InsufficientPayment);

        // Route fee (if any) using a short immutable borrow of market AccountInfo (no &mut held)
        if treasury_opt.is_some() && fee_fp > 0 {
            let seeds = [
                SEED_MARKET,
                pda_authority.as_ref(),
                milestone_id.as_ref(),
                &[bump],
            ];
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault_usdc.to_account_info(),
                        to: ctx.accounts.treasury_usdc.to_account_info(),
                        authority: ctx.accounts.market.to_account_info(),
                    },
                    &[&seeds],
                ),
                fee_fp as u64,
            )?;
        }

        // Now take a fresh mutable borrow to update market + position
        {
            let m = &mut ctx.accounts.market;
            match side {
                Side::Hit => {
                    m.q_hit_fp = m.q_hit_fp.checked_add(delta_q).ok_or(AmmError::MathOverflow)?;
                    pos.hit_shares_fp =
                        pos.hit_shares_fp.checked_add(delta_q).ok_or(AmmError::MathOverflow)?;
                    require!(pos.hit_shares_fp <= m.max_position_shares_fp, AmmError::PositionTooLarge);
                }
                Side::Miss => {
                    m.q_miss_fp = m.q_miss_fp.checked_add(delta_q).ok_or(AmmError::MathOverflow)?;
                    pos.miss_shares_fp =
                        pos.miss_shares_fp.checked_add(delta_q).ok_or(AmmError::MathOverflow)?;
                    require!(pos.miss_shares_fp <= m.max_position_shares_fp, AmmError::PositionTooLarge);
                }
            }
            let p_hit = lmsr_price_hit(m.b_fp, m.q_hit_fp, m.q_miss_fp)?;
            emit!(TradeEvent {
                market: m.key(),
                user: ctx.accounts.user.key(),
                side,
                is_buy: true,
                usdc_fp: dcost_fp as u64,
                shares_fp: delta_q as u64,
                fee_fp: fee_fp as u64,
                p_hit_milli: (p_hit * (PRICE_MILLI_SCALER as f64)) as i64,
            });
        }
        Ok(())
    }

    /// Sell virtual shares back to the AMM; user receives USDC minus fee.
    pub fn sell(
        ctx: Context<Trade>,
        side: Side,
        shares_in_fp: u64,
        min_usdc_out_fp: u64,
    ) -> Result<()> {
        // Snapshot reads
        let clock = Clock::get()?;
        let paused = ctx.accounts.market.paused;
        let outcome = ctx.accounts.market.outcome;
        let deadline_ts = ctx.accounts.market.deadline_ts;
        let usdc_mint = ctx.accounts.market.usdc_mint;
        let vault_usdc_pk = ctx.accounts.market.vault_usdc;
        let b_fp = ctx.accounts.market.b_fp;
        let fee_bps = ctx.accounts.market.fee_bps;
        let q_hit0 = ctx.accounts.market.q_hit_fp;
        let q_miss0 = ctx.accounts.market.q_miss_fp;
        let pda_authority = ctx.accounts.market.authority;
        let bump = ctx.accounts.market.bump;
        let milestone_id = ctx.accounts.market.milestone_id.clone();
        let treasury_opt = ctx.accounts.market.treasury;

        // Checks
        require!(!paused, AmmError::Paused);
        require!(outcome == Outcome::Unresolved, AmmError::AlreadySettled);
        require!(clock.unix_timestamp < deadline_ts, AmmError::AfterDeadline);
        require!(ctx.accounts.user_usdc.owner == ctx.accounts.user.key(), AmmError::InvalidOwner);
        require!(ctx.accounts.user_usdc.mint == usdc_mint, AmmError::WrongMint);
        require!(ctx.accounts.vault_usdc.key() == vault_usdc_pk, AmmError::WrongVault);

        let pos = &mut ctx.accounts.position;
        require!(pos.owner == ctx.accounts.user.key(), AmmError::Unauthorized);
        require!(pos.market == ctx.accounts.market.key(), AmmError::WrongMarket);

        let delta_q = shares_in_fp as i128;
        require!(delta_q > 0, AmmError::InvalidAmount);

        match side {
            Side::Hit => require!(pos.hit_shares_fp >= delta_q, AmmError::InsufficientBalance),
            Side::Miss => require!(pos.miss_shares_fp >= delta_q, AmmError::InsufficientBalance),
        }

        // ΔC for decreasing quantity (negative delta inside helper)
        let dcost_fp = lmsr_delta_cost(b_fp, q_hit0, q_miss0, side, -delta_q)?;
        let fee_fp = (dcost_fp as i128)
            .checked_mul(fee_bps as i128)
            .ok_or(AmmError::MathOverflow)?
            / 10_000i128;
        let payout_fp = (dcost_fp as i128)
            .checked_sub(fee_fp)
            .ok_or(AmmError::MathOverflow)?;
        require!(payout_fp >= (min_usdc_out_fp as i128), AmmError::Slippage);

        // Pay user from vault (market PDA is authority) — in its own scope (no &mut market held)
        if payout_fp > 0 {
            let seeds = [
                SEED_MARKET,
                pda_authority.as_ref(),
                milestone_id.as_ref(),
                &[bump],
            ];
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault_usdc.to_account_info(),
                        to: ctx.accounts.user_usdc.to_account_info(),
                        authority: ctx.accounts.market.to_account_info(),
                    },
                    &[&seeds],
                ),
                payout_fp as u64,
            )?;
        }

        // Fee to treasury if any
        if treasury_opt.is_some() && fee_fp > 0 {
            let seeds = [
                SEED_MARKET,
                pda_authority.as_ref(),
                milestone_id.as_ref(),
                &[bump],
            ];
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.vault_usdc.to_account_info(),
                        to: ctx.accounts.treasury_usdc.to_account_info(),
                        authority: ctx.accounts.market.to_account_info(),
                    },
                    &[&seeds],
                ),
                fee_fp as u64,
            )?;
        }

        // Now mutate market and position
        {
            let m = &mut ctx.accounts.market;
            match side {
                Side::Hit => {
                    m.q_hit_fp = m.q_hit_fp.checked_sub(delta_q).ok_or(AmmError::MathOverflow)?;
                    pos.hit_shares_fp =
                        pos.hit_shares_fp.checked_sub(delta_q).ok_or(AmmError::MathOverflow)?;
                }
                Side::Miss => {
                    m.q_miss_fp = m.q_miss_fp.checked_sub(delta_q).ok_or(AmmError::MathOverflow)?;
                    pos.miss_shares_fp =
                        pos.miss_shares_fp.checked_sub(delta_q).ok_or(AmmError::MathOverflow)?;
                }
            }
            let p_hit = lmsr_price_hit(m.b_fp, m.q_hit_fp, m.q_miss_fp)?;
            emit!(TradeEvent {
                market: m.key(),
                user: ctx.accounts.user.key(),
                side,
                is_buy: false,
                usdc_fp: dcost_fp as u64,
                shares_fp: shares_in_fp,
                fee_fp: fee_fp as u64,
                p_hit_milli: (p_hit * (PRICE_MILLI_SCALER as f64)) as i64,
            });
        }
        Ok(())
    }

    /// Settle the market to Hit or Miss once the window passes.
    pub fn settle_market(ctx: Context<Settle>, outcome: Outcome) -> Result<()> {
        let clock = Clock::get()?;
        let deadline_ts = ctx.accounts.market.deadline_ts;
        let grace = ctx.accounts.market.grace_period_secs;
        require!(ctx.accounts.market.outcome == Outcome::Unresolved, AmmError::AlreadySettled);
        require!(
            clock.unix_timestamp >= deadline_ts + grace,
            AmmError::BeforeSettlementWindow
        );

        // Authorization: authority or oracle (if configured)
        let mut authorized = ctx.accounts.authority.is_some()
            && ctx.accounts.authority.as_ref().unwrap().key().eq(&ctx.accounts.market.authority);
        if let Some(or) = &ctx.accounts.market.oracle_signer {
            if !authorized {
                authorized = ctx.accounts.oracle_signer.is_some()
                    && ctx.accounts.oracle_signer.as_ref().unwrap().key.eq(or);
            }
        }
        require!(authorized, AmmError::Unauthorized);
        require!(matches!(outcome, Outcome::Hit | Outcome::Miss), AmmError::InvalidOutcome);

        let m = &mut ctx.accounts.market;
        m.outcome = outcome;
        m.paused = true;

        emit!(Settled { market: m.key(), outcome });
        Ok(())
    }

    /// Redeem winning shares for USDC @ 1.0 per share after settlement.
    pub fn redeem(ctx: Context<Redeem>) -> Result<()> {
        require!(ctx.accounts.market.outcome != Outcome::Unresolved, AmmError::Unsettled);
        require!(ctx.accounts.position.owner == ctx.accounts.user.key(), AmmError::Unauthorized);
        require!(ctx.accounts.position.market == ctx.accounts.market.key(), AmmError::WrongMarket);

        // Snapshot fields needed for CPI
        let outcome = ctx.accounts.market.outcome;
        let pda_authority = ctx.accounts.market.authority;
        let bump = ctx.accounts.market.bump;
        let milestone_id = ctx.accounts.market.milestone_id.clone();

        // Compute redemption amounts and zero out shares with a mutable borrow of position only
        let redeem_fp = match outcome {
            Outcome::Hit => {
                let amt = ctx.accounts.position.hit_shares_fp;
                ctx.accounts.position.hit_shares_fp = 0;
                ctx.accounts.position.miss_shares_fp = 0;
                amt
            }
            Outcome::Miss => {
                let amt = ctx.accounts.position.miss_shares_fp;
                ctx.accounts.position.hit_shares_fp = 0;
                ctx.accounts.position.miss_shares_fp = 0;
                amt
            }
            Outcome::Unresolved => unreachable!(),
        };
        if redeem_fp <= 0 {
            return Ok(());
        }

        // Pay from vault to user (no &mut market held while doing CPI)
        let seeds = [
            SEED_MARKET,
            pda_authority.as_ref(),
            milestone_id.as_ref(),
            &[bump],
        ];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault_usdc.to_account_info(),
                    to: ctx.accounts.user_usdc.to_account_info(),
                    authority: ctx.accounts.market.to_account_info(),
                },
                &[&seeds],
            ),
            redeem_fp as u64,
        )?;

        emit!(Redeemed {
            market: ctx.accounts.market.key(),
            user: ctx.accounts.user.key(),
            amount_fp: redeem_fp as u64
        });
        Ok(())
    }

    pub fn admin_set_paused(ctx: Context<AdminAuth>, paused: bool) -> Result<()> {
        require!(ctx.accounts.authority.key() == ctx.accounts.market.authority, AmmError::Unauthorized);
        let m = &mut ctx.accounts.market;
        m.paused = paused;
        emit!(Paused { market: m.key(), paused });
        Ok(())
    }

    pub fn admin_update_params(ctx: Context<AdminAuth>, upd: UpdateParams) -> Result<()> {
        require!(ctx.accounts.authority.key() == ctx.accounts.market.authority, AmmError::Unauthorized);
        let m = &mut ctx.accounts.market;
        if let Some(b_fp) = upd.b_fp {
            require!(b_fp >= 10_000 && b_fp <= 1_000_000_000_000, AmmError::InvalidB);
            m.b_fp = b_fp as i128;
        }
        if let Some(fee_bps) = upd.fee_bps {
            require!(fee_bps <= 10_000, AmmError::InvalidFee);
            m.fee_bps = fee_bps;
        }
        if let Some(deadline_ts) = upd.deadline_ts {
            require!(deadline_ts >= m.deadline_ts, AmmError::InvalidUpdate);
            m.deadline_ts = deadline_ts;
        }
        if let Some(grace) = upd.grace_period_secs {
            m.grace_period_secs = grace;
        }
        if let Some(max_trade) = upd.max_trade_usdc_fp {
            m.max_trade_usdc_fp = max_trade as i128;
        }
        if let Some(max_pos) = upd.max_position_shares_fp {
            m.max_position_shares_fp = max_pos as i128;
        }
        if let Some(treasury) = upd.treasury {
            m.treasury = Some(treasury);
        }
        if let Some(oracle) = upd.oracle_signer {
            m.oracle_signer = Some(oracle);
        }
        Ok(())
    }
}

/// ========== Accounts ==========

#[derive(Accounts)]
#[instruction(params: InitParams, milestone_id: Vec<u8>)]
pub struct InitMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    pub usdc_mint: Account<'info, Mint>,

    /// Vault ATA owned by the market PDA (created here)
    #[account(
        init,
        payer = authority,
        associated_token::mint = usdc_mint,
        associated_token::authority = market
    )]
    pub vault_usdc: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = authority,
        seeds = [SEED_MARKET, authority.key().as_ref(), milestone_id.as_ref()],
        bump,
        space = 8 + Market::SIZE
    )]
    pub market: Account<'info, Market>,

    pub system_program: Program<'info, System>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub token_program: Program<'info, Token>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct SeedLiquidity<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,

    #[account(
        mut,
        constraint = authority_usdc.owner == authority.key(),
        constraint = authority_usdc.mint == market.usdc_mint
    )]
    pub authority_usdc: Account<'info, TokenAccount>,

    #[account(mut, address = market.vault_usdc)]
    pub vault_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Trade<'info> {
    /// User placing trade
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,

    /// User USDC ATA
    #[account(
        mut,
        constraint = user_usdc.owner == user.key(),
        constraint = user_usdc.mint == market.usdc_mint
    )]
    pub user_usdc: Account<'info, TokenAccount>,

    /// Vault USDC ATA owned by market PDA
    #[account(mut, address = market.vault_usdc)]
    pub vault_usdc: Account<'info, TokenAccount>,

    /// Position PDA for (market, user)
    #[account(
        init_if_needed,
        payer = user,
        seeds = [SEED_POSITION, market.key().as_ref(), user.key().as_ref()],
        bump,
        space = 8 + Position::SIZE
    )]
    pub position: Account<'info, Position>,

    /// Optional treasury ATA (required iff market.treasury.is_some())
    #[account(mut)]
    pub treasury_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Settle<'info> {
    #[account(mut)]
    pub market: Account<'info, Market>,

    /// Either authority signer …
    pub authority: Option<Signer<'info>>,
    /// …or oracle signer if configured
    pub oracle_signer: Option<Signer<'info>>,
}

#[derive(Accounts)]
pub struct Redeem<'info> {
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(mut)]
    pub market: Account<'info, Market>,

    #[account(
        mut,
        seeds = [SEED_POSITION, market.key().as_ref(), user.key().as_ref()],
        bump
    )]
    pub position: Account<'info, Position>,

    #[account(
        mut,
        constraint = user_usdc.owner == user.key(),
        constraint = user_usdc.mint == market.usdc_mint
    )]
    pub user_usdc: Account<'info, TokenAccount>,

    #[account(mut, address = market.vault_usdc)]
    pub vault_usdc: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AdminAuth<'info> {
    pub authority: Signer<'info>,
    #[account(mut)]
    pub market: Account<'info, Market>,
}

/// ========== State ==========

#[account]
pub struct Market {
    pub authority: Pubkey,
    pub usdc_mint: Pubkey,
    pub vault_usdc: Pubkey,
    pub b_fp: i128,
    pub fee_bps: u16,
    pub deadline_ts: i64,
    pub grace_period_secs: i64,
    pub outcome: Outcome,
    pub q_hit_fp: i128,
    pub q_miss_fp: i128,
    pub paused: bool,
    pub max_trade_usdc_fp: i128,
    pub max_position_shares_fp: i128,
    pub treasury: Option<Pubkey>,
    pub milestone_id: Vec<u8>,
    pub liquidity_usdc_fp: i128,
    pub oracle_signer: Option<Pubkey>,
    pub bump: u8,
}
impl Market {
    // conservative bound; adjust if Anchor complains about space
    pub const SIZE: usize =
        32 + 32 + 32 + 16 + 2 + 8 + 8 + 1 + 16 + 16 + 1 + 16 + 16 + 1 + 4 + 64 + 16 + 1 + 32 + 1;
}

#[account]
pub struct Position {
    pub owner: Pubkey,
    pub market: Pubkey,
    pub hit_shares_fp: i128,
    pub miss_shares_fp: i128,
}
impl Position {
    pub const SIZE: usize = 32 + 32 + 16 + 16;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Unresolved,
    Hit,
    Miss,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Hit,
    Miss,
}

/// ========== Params DTOs ==========

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitParams {
    pub b_fp: u64,
    pub fee_bps: u16,
    pub deadline_ts: i64,
    pub grace_period_secs: i64,
    pub max_trade_usdc_fp: u64,
    pub max_position_shares_fp: u64,
    pub treasury: Option<Pubkey>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct UpdateParams {
    pub b_fp: Option<u64>,
    pub fee_bps: Option<u16>,
    pub deadline_ts: Option<i64>,
    pub grace_period_secs: Option<i64>,
    pub max_trade_usdc_fp: Option<u64>,
    pub max_position_shares_fp: Option<u64>,
    pub treasury: Option<Pubkey>,
    pub oracle_signer: Option<Pubkey>,
}

/// ========== Events ==========

#[event]
pub struct MarketInitialized {
    pub market: Pubkey,
    pub b_fp: i64,
    pub fee_bps: u16,
    pub deadline_ts: i64,
}

#[event]
pub struct TradeEvent {
    pub market: Pubkey,
    pub user: Pubkey,
    pub side: Side,
    pub is_buy: bool,
    pub usdc_fp: u64,
    pub shares_fp: u64,
    pub fee_fp: u64,
    pub p_hit_milli: i64,
}

#[event]
pub struct Settled {
    pub market: Pubkey,
    pub outcome: Outcome,
}

#[event]
pub struct Redeemed {
    pub market: Pubkey,
    pub user: Pubkey,
    pub amount_fp: u64,
}

#[event]
pub struct Paused {
    pub market: Pubkey,
    pub paused: bool,
}

/// ========== Errors ==========

#[error_code]
pub enum AmmError {
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Trade exceeds max_trade_usdc_fp")]
    TradeTooLarge,
    #[msg("Position exceeds max_position_shares_fp")]
    PositionTooLarge,
    #[msg("After deadline")]
    AfterDeadline,
    #[msg("Before settlement window")]
    BeforeSettlementWindow,
    #[msg("Invalid outcome")]
    InvalidOutcome,
    #[msg("Insufficient balance")]
    InsufficientBalance,
    #[msg("Slippage / constraint not met")]
    Slippage,
    #[msg("Market is paused")]
    Paused,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Already settled")]
    AlreadySettled,
    #[msg("Market not yet settled")]
    Unsettled,
    #[msg("Invalid update")]
    InvalidUpdate,
    #[msg("Invalid B parameter")]
    InvalidB,
    #[msg("Invalid fee")]
    InvalidFee,
    #[msg("Wrong vault account")]
    WrongVault,
    #[msg("Wrong mint")]
    WrongMint,
    #[msg("Invalid owner")]
    InvalidOwner,
    #[msg("Wrong market for position")]
    WrongMarket,
    #[msg("Insufficient payment collected")]
    InsufficientPayment,
    #[msg("Invalid amount")]
    InvalidAmount,
}

/// ========== Math Helpers (LMSR) ==========

fn position_side_shares(pos: &Position, side: Side) -> i128 {
    match side {
        Side::Hit => pos.hit_shares_fp,
        Side::Miss => pos.miss_shares_fp,
    }
}

/// LMSR price of HIT given b, q_hit, q_miss (all in fp)
fn lmsr_price_hit(b_fp: i128, q_hit_fp: i128, q_miss_fp: i128) -> Result<f64> {
    let b = (b_fp as f64) / (FP_SCALER as f64);
    let x = (q_hit_fp as f64) / (FP_SCALER as f64) / b;
    let y = (q_miss_fp as f64) / (FP_SCALER as f64) / b;

    // Stabilized softmax
    let m = x.max(y);
    let ex = (x - m).exp();
    let ey = (y - m).exp();
    let denom = ex + ey;
    require!(denom.is_finite() && denom > 0.0, AmmError::MathOverflow);
    Ok(ex / denom)
}

/// LMSR cost C(q) in fp USDC
fn lmsr_cost(b_fp: i128, q_hit_fp: i128, q_miss_fp: i128) -> Result<i128> {
    let b = (b_fp as f64) / (FP_SCALER as f64);
    let x = (q_hit_fp as f64) / (FP_SCALER as f64) / b;
    let y = (q_miss_fp as f64) / (FP_SCALER as f64) / b;

    let m = x.max(y);
    let sum = (x - m).exp() + (y - m).exp();
    require!(sum.is_finite() && sum > 0.0, AmmError::MathOverflow);

    let c = b * (m + sum.ln());
    let c_fp = (c * FP_SCALER as f64).round() as i128;
    Ok(c_fp)
}

/// ΔC = C(q + d) - C(q) (fp)
fn lmsr_delta_cost(
    b_fp: i128,
    q_hit_fp: i128,
    q_miss_fp: i128,
    side: Side,
    delta_q_fp: i128,
) -> Result<i128> {
    let (qh1, qm1) = match side {
        Side::Hit => (
            q_hit_fp.checked_add(delta_q_fp).ok_or(AmmError::MathOverflow)?,
            q_miss_fp,
        ),
        Side::Miss => (
            q_hit_fp,
            q_miss_fp.checked_add(delta_q_fp).ok_or(AmmError::MathOverflow)?,
        ),
    };
    require!(qh1 >= 0 && qm1 >= 0, AmmError::MathOverflow);

    let c0 = lmsr_cost(b_fp, q_hit_fp, q_miss_fp)?;
    let c1 = lmsr_cost(b_fp, qh1, qm1)?;
    let d = c1.checked_sub(c0).ok_or(AmmError::MathOverflow)?;
    Ok(d)
}

/// Solve for Δq >= 0 such that ΔC == target_fp (buy), with bisection and caps.
fn solve_delta_q_bisect(
    b_fp: i128,
    q_hit_fp: i128,
    q_miss_fp: i128,
    side: Side,
    target_fp: i128,
    max_pos_fp: i128,
    current_pos_fp: i128,
) -> Result<i128> {
    require!(target_fp >= 0, AmmError::MathOverflow);
    if target_fp == 0 {
        return Ok(0);
    }
    let max_delta_pos = max_pos_fp.checked_sub(current_pos_fp).ok_or(AmmError::MathOverflow)?;
    require!(max_delta_pos > 0, AmmError::PositionTooLarge);

    // Exponential search for a reasonable upper bound on Δq
    let mut lo: i128 = 0;
    let mut hi: i128 = max_delta_pos.min(1000 * target_fp); // heuristic
    for _ in 0..20 {
        let dcost = lmsr_delta_cost(b_fp, q_hit_fp, q_miss_fp, side, hi)?;
        if dcost >= target_fp {
            break;
        }
        hi = hi.saturating_mul(2).min(max_delta_pos);
        if hi == max_delta_pos {
            break;
        }
    }

    // Bisection
    let mut res = hi;
    for _ in 0..MAX_BISECT_ITERS {
        let mid = lo + ((hi - lo) / 2);
        let dcost = lmsr_delta_cost(b_fp, q_hit_fp, q_miss_fp, side, mid)?;
        if dcost >= target_fp {
            res = mid;
            hi = mid;
        } else {
            lo = mid + 1;
        }
        if hi <= lo {
            res = hi;
            break;
        }
    }
    Ok(res)
}

/// ========== Utilities ==========

impl<'info> Trade<'info> {
    /// Example of signer seeds builder (now lifetime-safe)
    fn _signer_seeds(&self) -> [&[u8]; 4] {
        // Make a slice that BORROWS the u8 stored inside `self.market.bump`
        // instead of creating a temporary `[u8]`.
        let bump_slice: &[u8] = core::slice::from_ref(&self.market.bump);
        [
            SEED_MARKET,
            self.market.authority.as_ref(),
            self.market.milestone_id.as_ref(),
            bump_slice,
        ]
    }
}
