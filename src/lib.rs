use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Burn, Mint, MintTo, Token, TokenAccount, TransferChecked};

declare_id!("Ax14TVY1fUXoU5ZagXdtQtMRnhAYugHdD9dzoG2UEG2M");

// pool-demo: a constant-product (x * y = k) AMM for two faucet test tokens.
// Every account that pays rent is `payer`; `user` is the wallet that owns the
// tokens. On the website both are the visitor's wallet.

pub const DECIMALS: u8 = 6;
pub const FAUCET_MAX: u64 = 10_000 * 1_000_000; // 10,000 tokens per claim, per token
pub const MAX_FEE_BPS: u16 = 100; // 1%
pub const MIN_LIQUIDITY: u64 = 1_000; // locked forever on the first deposit
const BPS: u128 = 10_000;

#[program]
pub mod pool_demo {
    use super::*;

    /// Creates the two test tokens (tUSD, tSOL), both minted by the faucet PDA.
    pub fn init_mints(ctx: Context<InitMints>) -> Result<()> {
        let f = &mut ctx.accounts.faucet;
        f.tusd = ctx.accounts.tusd.key();
        f.tsol = ctx.accounts.tsol.key();
        f.bump = ctx.bumps.faucet;
        Ok(())
    }

    /// Mints up to 10,000 tUSD and 10,000 tSOL (base units, 6 decimals) to `recipient`.
    pub fn faucet(ctx: Context<FaucetClaim>, amount_tusd: u64, amount_tsol: u64) -> Result<()> {
        require!(amount_tusd <= FAUCET_MAX && amount_tsol <= FAUCET_MAX, PoolError::FaucetLimit);
        require!(amount_tusd > 0 || amount_tsol > 0, PoolError::ZeroAmount);
        let bump = ctx.accounts.faucet.bump;
        let seeds: &[&[u8]] = &[b"faucet", &[bump]];
        let tp = ctx.accounts.token_program.key();
        if amount_tusd > 0 {
            token::mint_to(
                CpiContext::new_with_signer(
                    tp,
                    MintTo {
                        mint: ctx.accounts.tusd.to_account_info(),
                        to: ctx.accounts.recipient_tusd.to_account_info(),
                        authority: ctx.accounts.faucet.to_account_info(),
                    },
                    &[seeds],
                ),
                amount_tusd,
            )?;
        }
        if amount_tsol > 0 {
            token::mint_to(
                CpiContext::new_with_signer(
                    tp,
                    MintTo {
                        mint: ctx.accounts.tsol.to_account_info(),
                        to: ctx.accounts.recipient_tsol.to_account_info(),
                        authority: ctx.accounts.faucet.to_account_info(),
                    },
                    &[seeds],
                ),
                amount_tsol,
            )?;
        }
        Ok(())
    }

    /// Creates an empty pool for mint_a / mint_b with a swap fee of up to 1% (100 bps).
    pub fn create_pool(ctx: Context<CreatePool>, fee_bps: u16) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, PoolError::FeeTooHigh);
        require_keys_neq!(ctx.accounts.mint_a.key(), ctx.accounts.mint_b.key(), PoolError::SameMint);
        let p = &mut ctx.accounts.pool;
        p.creator = ctx.accounts.creator.key();
        p.mint_a = ctx.accounts.mint_a.key();
        p.mint_b = ctx.accounts.mint_b.key();
        p.vault_a = ctx.accounts.vault_a.key();
        p.vault_b = ctx.accounts.vault_b.key();
        p.lp_mint = ctx.accounts.lp_mint.key();
        p.fee_bps = fee_bps;
        p.reserve_a = 0;
        p.reserve_b = 0;
        p.lp_supply = 0;
        p.bump = ctx.bumps.pool;
        emit!(PoolCreated {
            pool: p.key(),
            creator: p.creator,
            mint_a: p.mint_a,
            mint_b: p.mint_b,
            fee_bps,
            ts: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Deposits at most `max_a` / `max_b`. The first deposit sets the price and
    /// mints sqrt(a * b) LP (minus MIN_LIQUIDITY, locked). Later deposits take only
    /// the amounts that match the pool's ratio and mint LP in proportion.
    pub fn add_liquidity(ctx: Context<Liquidity>, max_a: u64, max_b: u64, min_lp: u64) -> Result<()> {
        require!(max_a > 0 && max_b > 0, PoolError::ZeroAmount);
        let p = &ctx.accounts.pool;
        let ra = p.reserve_a as u128;
        let rb = p.reserve_b as u128;
        let s = p.lp_supply as u128;

        let (lp_total, minted, a, b) = if s == 0 {
            let lp = (max_a as u128)
                .checked_mul(max_b as u128)
                .ok_or(PoolError::MathOverflow)?
                .isqrt();
            require!(lp > MIN_LIQUIDITY as u128, PoolError::InitialLiquidityTooSmall);
            (lp, lp - MIN_LIQUIDITY as u128, max_a as u128, max_b as u128)
        } else {
            let lp_a = mul_div(max_a as u128, s, ra)?;
            let lp_b = mul_div(max_b as u128, s, rb)?;
            let lp = lp_a.min(lp_b);
            require!(lp > 0, PoolError::ZeroAmount);
            let a = ceil_mul_div(lp, ra, s)?;
            let b = ceil_mul_div(lp, rb, s)?;
            (lp, lp, a, b)
        };
        let a = to_u64(a)?;
        let b = to_u64(b)?;
        let minted = to_u64(minted)?;
        require!(a <= max_a && b <= max_b, PoolError::MathOverflow);
        require!(minted >= min_lp, PoolError::SlippageExceeded);

        let new_ra = to_u64(ra.checked_add(a as u128).ok_or(PoolError::MathOverflow)?)?;
        let new_rb = to_u64(rb.checked_add(b as u128).ok_or(PoolError::MathOverflow)?)?;
        let new_s = to_u64(s.checked_add(lp_total).ok_or(PoolError::MathOverflow)?)?;

        let acc = &ctx.accounts;
        let tp = acc.token_program.key();
        let user = acc.user.to_account_info();
        token::transfer_checked(
            CpiContext::new(
                tp,
                TransferChecked {
                    from: acc.user_a.to_account_info(),
                    mint: acc.mint_a.to_account_info(),
                    to: acc.vault_a.to_account_info(),
                    authority: user.clone(),
                },
            ),
            a,
            acc.mint_a.decimals,
        )?;
        token::transfer_checked(
            CpiContext::new(
                tp,
                TransferChecked {
                    from: acc.user_b.to_account_info(),
                    mint: acc.mint_b.to_account_info(),
                    to: acc.vault_b.to_account_info(),
                    authority: user,
                },
            ),
            b,
            acc.mint_b.decimals,
        )?;
        let (ka, kb, bump) = (p.mint_a, p.mint_b, p.bump);
        let seeds: &[&[u8]] = &[b"pool", ka.as_ref(), kb.as_ref(), &[bump]];
        token::mint_to(
            CpiContext::new_with_signer(
                tp,
                MintTo {
                    mint: acc.lp_mint.to_account_info(),
                    to: acc.user_lp.to_account_info(),
                    authority: acc.pool.to_account_info(),
                },
                &[seeds],
            ),
            minted,
        )?;

        let p = &mut ctx.accounts.pool;
        p.reserve_a = new_ra;
        p.reserve_b = new_rb;
        p.lp_supply = new_s;
        emit!(LiquidityEvent {
            pool: p.key(),
            user: ctx.accounts.user.key(),
            is_add: true,
            amount_a: a,
            amount_b: b,
            lp: minted,
            reserve_a: new_ra,
            reserve_b: new_rb,
            lp_supply: new_s,
            ts: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Burns `lp_amount` LP and pays out the pro-rata share of both reserves.
    pub fn remove_liquidity(ctx: Context<Liquidity>, lp_amount: u64, min_a: u64, min_b: u64) -> Result<()> {
        require!(lp_amount > 0, PoolError::ZeroAmount);
        let p = &ctx.accounts.pool;
        let s = p.lp_supply as u128;
        require!(s > 0, PoolError::EmptyPool);
        let a = to_u64(mul_div(lp_amount as u128, p.reserve_a as u128, s)?)?;
        let b = to_u64(mul_div(lp_amount as u128, p.reserve_b as u128, s)?)?;
        require!(a > 0 || b > 0, PoolError::ZeroAmount);
        require!(a >= min_a && b >= min_b, PoolError::SlippageExceeded);
        let new_ra = p.reserve_a.checked_sub(a).ok_or(PoolError::MathOverflow)?;
        let new_rb = p.reserve_b.checked_sub(b).ok_or(PoolError::MathOverflow)?;
        let new_s = p.lp_supply.checked_sub(lp_amount).ok_or(PoolError::MathOverflow)?;

        let acc = &ctx.accounts;
        let tp = acc.token_program.key();
        token::burn(
            CpiContext::new(
                tp,
                Burn {
                    mint: acc.lp_mint.to_account_info(),
                    from: acc.user_lp.to_account_info(),
                    authority: acc.user.to_account_info(),
                },
            ),
            lp_amount,
        )?;
        let (ka, kb, bump) = (p.mint_a, p.mint_b, p.bump);
        let seeds: &[&[u8]] = &[b"pool", ka.as_ref(), kb.as_ref(), &[bump]];
        let pool_info = acc.pool.to_account_info();
        if a > 0 {
            token::transfer_checked(
                CpiContext::new_with_signer(
                    tp,
                    TransferChecked {
                        from: acc.vault_a.to_account_info(),
                        mint: acc.mint_a.to_account_info(),
                        to: acc.user_a.to_account_info(),
                        authority: pool_info.clone(),
                    },
                    &[seeds],
                ),
                a,
                acc.mint_a.decimals,
            )?;
        }
        if b > 0 {
            token::transfer_checked(
                CpiContext::new_with_signer(
                    tp,
                    TransferChecked {
                        from: acc.vault_b.to_account_info(),
                        mint: acc.mint_b.to_account_info(),
                        to: acc.user_b.to_account_info(),
                        authority: pool_info,
                    },
                    &[seeds],
                ),
                b,
                acc.mint_b.decimals,
            )?;
        }

        let p = &mut ctx.accounts.pool;
        p.reserve_a = new_ra;
        p.reserve_b = new_rb;
        p.lp_supply = new_s;
        emit!(LiquidityEvent {
            pool: p.key(),
            user: ctx.accounts.user.key(),
            is_add: false,
            amount_a: a,
            amount_b: b,
            lp: lp_amount,
            reserve_a: new_ra,
            reserve_b: new_rb,
            lp_supply: new_s,
            ts: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Swaps `amount_in` of A for B (a_to_b) or B for A. The fee stays in the pool.
    /// Fails with SlippageExceeded when the output would be below `min_out`.
    pub fn swap(ctx: Context<Swap>, a_to_b: bool, amount_in: u64, min_out: u64) -> Result<()> {
        require!(amount_in > 0, PoolError::ZeroAmount);
        let p = &ctx.accounts.pool;
        let (rin, rout) = if a_to_b {
            (p.reserve_a as u128, p.reserve_b as u128)
        } else {
            (p.reserve_b as u128, p.reserve_a as u128)
        };
        require!(rin > 0 && rout > 0, PoolError::EmptyPool);
        let fee = ceil_mul_div(amount_in as u128, p.fee_bps as u128, BPS)?;
        let net_in = (amount_in as u128).checked_sub(fee).ok_or(PoolError::MathOverflow)?;
        let out = mul_div(rout, net_in, rin.checked_add(net_in).ok_or(PoolError::MathOverflow)?)?;
        require!(out > 0, PoolError::OutputTooSmall);
        require!(out >= min_out as u128, PoolError::SlippageExceeded);
        let out = to_u64(out)?;
        let fee = to_u64(fee)?;
        let new_in = to_u64(rin.checked_add(amount_in as u128).ok_or(PoolError::MathOverflow)?)?;
        let new_out = to_u64(rout - out as u128)?;
        let (new_ra, new_rb) = if a_to_b { (new_in, new_out) } else { (new_out, new_in) };

        let acc = &ctx.accounts;
        let tp = acc.token_program.key();
        let (user_in, user_out, vault_in, vault_out, mint_in, mint_out) = if a_to_b {
            (&acc.user_a, &acc.user_b, &acc.vault_a, &acc.vault_b, &acc.mint_a, &acc.mint_b)
        } else {
            (&acc.user_b, &acc.user_a, &acc.vault_b, &acc.vault_a, &acc.mint_b, &acc.mint_a)
        };
        token::transfer_checked(
            CpiContext::new(
                tp,
                TransferChecked {
                    from: user_in.to_account_info(),
                    mint: mint_in.to_account_info(),
                    to: vault_in.to_account_info(),
                    authority: acc.user.to_account_info(),
                },
            ),
            amount_in,
            mint_in.decimals,
        )?;
        let (ka, kb, bump) = (p.mint_a, p.mint_b, p.bump);
        let seeds: &[&[u8]] = &[b"pool", ka.as_ref(), kb.as_ref(), &[bump]];
        token::transfer_checked(
            CpiContext::new_with_signer(
                tp,
                TransferChecked {
                    from: vault_out.to_account_info(),
                    mint: mint_out.to_account_info(),
                    to: user_out.to_account_info(),
                    authority: acc.pool.to_account_info(),
                },
                &[seeds],
            ),
            out,
            mint_out.decimals,
        )?;

        let p = &mut ctx.accounts.pool;
        p.reserve_a = new_ra;
        p.reserve_b = new_rb;
        emit!(SwapEvent {
            pool: p.key(),
            user: ctx.accounts.user.key(),
            a_to_b,
            amount_in,
            amount_out: out,
            fee,
            reserve_a: new_ra,
            reserve_b: new_rb,
            ts: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }
}

fn mul_div(x: u128, y: u128, d: u128) -> Result<u128> {
    require!(d > 0, PoolError::EmptyPool);
    Ok(x.checked_mul(y).ok_or(PoolError::MathOverflow)? / d)
}

fn ceil_mul_div(x: u128, y: u128, d: u128) -> Result<u128> {
    require!(d > 0, PoolError::EmptyPool);
    let n = x.checked_mul(y).ok_or(PoolError::MathOverflow)?;
    Ok(n.div_ceil(d))
}

fn to_u64(x: u128) -> Result<u64> {
    u64::try_from(x).map_err(|_| error!(PoolError::MathOverflow))
}

#[derive(Accounts)]
pub struct InitMints<'info> {
    #[account(init, payer = payer, space = 8 + Faucet::INIT_SPACE, seeds = [b"faucet"], bump)]
    pub faucet: Account<'info, Faucet>,
    #[account(
        init,
        payer = payer,
        seeds = [b"tusd"],
        bump,
        mint::decimals = DECIMALS,
        mint::authority = faucet,
    )]
    pub tusd: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = payer,
        seeds = [b"tsol"],
        bump,
        mint::decimals = DECIMALS,
        mint::authority = faucet,
    )]
    pub tsol: Box<Account<'info, Mint>>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct FaucetClaim<'info> {
    #[account(seeds = [b"faucet"], bump = faucet.bump)]
    pub faucet: Account<'info, Faucet>,
    #[account(mut, seeds = [b"tusd"], bump)]
    pub tusd: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"tsol"], bump)]
    pub tsol: Box<Account<'info, Mint>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = tusd,
        associated_token::authority = recipient,
    )]
    pub recipient_tusd: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = tsol,
        associated_token::authority = recipient,
    )]
    pub recipient_tsol: Box<Account<'info, TokenAccount>>,
    /// CHECK: any wallet can receive faucet tokens; it only sets the token accounts' owner
    pub recipient: UncheckedAccount<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CreatePool<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + Pool::INIT_SPACE,
        seeds = [b"pool", mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump
    )]
    pub pool: Box<Account<'info, Pool>>,
    pub mint_a: Box<Account<'info, Mint>>,
    pub mint_b: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = payer,
        seeds = [b"lp", pool.key().as_ref()],
        bump,
        mint::decimals = DECIMALS,
        mint::authority = pool,
    )]
    pub lp_mint: Box<Account<'info, Mint>>,
    #[account(
        init,
        payer = payer,
        seeds = [b"vault_a", pool.key().as_ref()],
        bump,
        token::mint = mint_a,
        token::authority = pool,
    )]
    pub vault_a: Box<Account<'info, TokenAccount>>,
    #[account(
        init,
        payer = payer,
        seeds = [b"vault_b", pool.key().as_ref()],
        bump,
        token::mint = mint_b,
        token::authority = pool,
    )]
    pub vault_b: Box<Account<'info, TokenAccount>>,
    pub creator: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Liquidity<'info> {
    #[account(
        mut,
        seeds = [b"pool", mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump,
        has_one = mint_a,
        has_one = mint_b,
        has_one = vault_a,
        has_one = vault_b,
        has_one = lp_mint,
    )]
    pub pool: Box<Account<'info, Pool>>,
    pub mint_a: Box<Account<'info, Mint>>,
    pub mint_b: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"lp", pool.key().as_ref()], bump)]
    pub lp_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"vault_a", pool.key().as_ref()], bump)]
    pub vault_a: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault_b", pool.key().as_ref()], bump)]
    pub vault_b: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_a,
        associated_token::authority = user,
    )]
    pub user_a: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_b,
        associated_token::authority = user,
    )]
    pub user_b: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = lp_mint,
        associated_token::authority = user,
    )]
    pub user_lp: Box<Account<'info, TokenAccount>>,
    pub user: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Swap<'info> {
    #[account(
        mut,
        seeds = [b"pool", mint_a.key().as_ref(), mint_b.key().as_ref()],
        bump = pool.bump,
        has_one = mint_a,
        has_one = mint_b,
        has_one = vault_a,
        has_one = vault_b,
    )]
    pub pool: Box<Account<'info, Pool>>,
    pub mint_a: Box<Account<'info, Mint>>,
    pub mint_b: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"vault_a", pool.key().as_ref()], bump)]
    pub vault_a: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [b"vault_b", pool.key().as_ref()], bump)]
    pub vault_b: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_a,
        associated_token::authority = user,
    )]
    pub user_a: Box<Account<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_b,
        associated_token::authority = user,
    )]
    pub user_b: Box<Account<'info, TokenAccount>>,
    pub user: Signer<'info>,
    #[account(mut)]
    pub payer: Signer<'info>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[account]
#[derive(InitSpace)]
pub struct Faucet {
    pub tusd: Pubkey,
    pub tsol: Pubkey,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub creator: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub lp_mint: Pubkey,
    pub fee_bps: u16,
    pub reserve_a: u64,
    pub reserve_b: u64,
    /// LP supply including the MIN_LIQUIDITY locked on the first deposit
    pub lp_supply: u64,
    pub bump: u8,
}

#[event]
pub struct PoolCreated {
    pub pool: Pubkey,
    pub creator: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub fee_bps: u16,
    pub ts: i64,
}

#[event]
pub struct SwapEvent {
    pub pool: Pubkey,
    pub user: Pubkey,
    pub a_to_b: bool,
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee: u64,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub ts: i64,
}

#[event]
pub struct LiquidityEvent {
    pub pool: Pubkey,
    pub user: Pubkey,
    pub is_add: bool,
    pub amount_a: u64,
    pub amount_b: u64,
    pub lp: u64,
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub lp_supply: u64,
    pub ts: i64,
}

#[error_code]
pub enum PoolError {
    #[msg("The faucet gives at most 10,000 of each token per claim")]
    FaucetLimit,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("The fee can be at most 1% (100 bps)")]
    FeeTooHigh,
    #[msg("A pool needs two different tokens")]
    SameMint,
    #[msg("The first deposit is too small")]
    InitialLiquidityTooSmall,
    #[msg("Price moved beyond your slippage limit")]
    SlippageExceeded,
    #[msg("The trade is too small to get any tokens out")]
    OutputTooSmall,
    #[msg("The pool has no liquidity")]
    EmptyPool,
    #[msg("Math overflow")]
    MathOverflow,
}
