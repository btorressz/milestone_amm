# milestone_amm

# 🎯 Milestone AMM

A Solana-based Automated Market Maker (AMM) for binary prediction markets built with Anchor. Users can trade virtual shares on milestone outcomes (**Hit** or **Miss**) using USDC, with prices determined by a **Logarithmic Market Scoring Rule (LMSR)**.

---

## 🧾 Overview

This program implements a prediction market where traders can:

- Buy and sell shares representing belief in a milestone being **Hit** or **Missed**
- Trade against an algorithmic market maker using **LMSR pricing**
- Redeem winning shares for **USDC** after market settlement

---

## ⚙️ Key Features

### 📈 LMSR Pricing

- **Logarithmic Market Scoring Rule** provides smooth, continuous pricing
- Market depth controlled by liquidity parameter `b`
- Prices automatically adjust based on share supply
- Built-in resistance to manipulation via price impact

### 💱 Trading Mechanics

- **Buy**: Purchase shares with USDC at current market price
- **Sell**: Sell shares back to AMM for USDC (minus fees)
- **Slippage Protection**: Enforced minimum output
- **Position Limits**: Configurable max trade and position sizes

### ⏳ Market Lifecycle

- **Initialization**: Authority sets up market parameters
- **Trading Period**: Users buy/sell shares before deadline
- **Grace Period**: Optional buffer post-deadline before settlement
- **Settlement**: Authority or oracle resolves market outcome
- **Redemption**: Winning side redeems shares for 1:1 USDC


## 🧮 Market Parameters

### Core Parameters

- `b_fp`: Liquidity parameter (e.g., 10,000 to 1,000,000,000,000 in fixed-point)
- `fee_bps`: Trading fee (0–10,000 = 0–100%)
- `deadline_ts`: Unix timestamp when trading closes
- `grace_period_secs`: Delay before settlement is allowed

### Safety Limits

- `max_trade_usdc_fp`: Max USDC per trade
- `max_position_shares_fp`: Max shares per user position

### Optional Features

- `treasury`: Optional account to collect fees
- `oracle_signer`: Optional oracle to settle the market

---

## 🛠️ Instructions

### `init_market`

Initialize a new prediction market.  
**Params**: Market authority, liquidity, deadline, fees, etc.

---

### `seed_liquidity`

Deposit USDC into market vault (authority only).

---

### `buy`

Buy shares on **Hit** or **Miss** side with USDC.

---

### `sell`

Sell shares back to the AMM to receive USDC (subject to fees).

---

### `settle_market`

Resolve the market outcome to **Hit** or **Miss** (authority or oracle only).

---

### `redeem`

Claim winnings by redeeming shares 1:1 for USDC after settlement.

---

## 🔧 Admin Functions

- `admin_set_paused`: Pause or unpause trading activity
- `admin_update_params`: Update core market parameters (e.g. fees, limits, deadlines)

---

## 🧠 Data Structures

### 🧾 Market Account

The primary state account for each market. Stores configuration and market status.

- `authority`: `Pubkey` — Market manager
- `b_fp`: Liquidity parameter in fixed-point
- `fee_bps`: Fee in basis points
- `q_hit_fp`: Outstanding Hit shares (fixed-point)
- `q_miss_fp`: Outstanding Miss shares (fixed-point)
- `outcome`: Market result (enum: Unresolved, Hit, Miss)
- `deadline_ts`: Trading deadline timestamp
- `grace_period_secs`: Required delay before settlement
- `vault`: Token account holding USDC
- `treasury`: Optional treasury account
- `oracle_signer`: Optional signer to settle outcome

---

### 👤 Position Account

Per-user account to track share holdings.

- `owner`: `Pubkey` — User wallet
- `hit_shares_fp`: Fixed-point amount of Hit shares
- `miss_shares_fp`: Fixed-point amount of Miss shares
- PDA: Derived from `[SEED_POSITION, market, user]`

---

### 🎭 Enums

- `Outcome`: `Unresolved | Hit | Miss`
- `Side`: `Hit | Miss`

---

## 💵 Fixed-Point Arithmetic

All USDC values and share quantities use **6-decimal fixed-point math**.

- `FP_SCALER = 1_000_000`
- `1 USDC = 1,000,000`
- Prices and values expressed in milli-units for high precision

---

## 💸 Fee Handling

- Fee = `(trade_cost × fee_bps) / 10,000`
- Total user payment = `trade_cost + fee`
- Fees routed to treasury account (if defined)
- Applies to both `buy` and `sell` instructions

---

## 🔐 Security Features

### 🛡️ Authorization

- Only market authority can perform admin actions
- Settlement can be performed by authority or designated oracle
- Users can only update their own positions

### 🧪 Safety Checks

- Market can be paused to halt trading
- Trading deadline enforced strictly
- Grace period must pass before settlement
- Slippage protection on all trades
- Overflow-resistant math throughout

### ✅ Borrow Checker Safety

- Ordered borrows to prevent runtime errors
- Snapshot pattern used for CPIs
- Avoids mixed mutable/immutable borrows

---

## 📊 Math Implementation

## 📐 LMSR Cost Function 

### 🎯 Cost Function

The **LMSR (Logarithmic Market Scoring Rule)** is a pricing formula used in prediction markets to ensure fair and dynamic pricing of outcome shares.

- The cost to buy shares depends on the **existing quantity of shares** on each side (Hit and Miss).
- The formula ensures **liquidity and continuous prices**, avoiding sudden price jumps.
- The cost increases as you buy more of one side, making it more expensive to push the price.

### 💸 Price Calculation

The **price of a share** (e.g. for "Hit") is derived by comparing the quantity of Hit and Miss shares using an exponential formula:

- If more Hit shares are bought, the price of Hit increases while Miss decreases.
- Prices are always between 0 and 1 and sum to 1, behaving like probabilities.
- The formula makes price changes **smooth and resistant to manipulation**.

### 🔄 Delta Cost (ΔC)

When a user makes a trade, we compute the **difference in cost** before and after the trade:

- ΔC = Cost after buying additional shares − Cost before trade
- This delta represents the **total USDC the user pays** to acquire the new shares.

### 🔍 Share Solving (Bisection Search)

To figure out **how many shares** a user gets for a specific USDC amount:

- The program uses a **bisection search algorithm**.
- It finds the number of shares such that the cost to buy them exactly equals the user’s input amount.
- This ensures **precise and fair pricing** even with complex math.

---

The combination of these math tools allows the AMM to offer **automated, fair, and liquid binary outcome trading** with strong resistance to price manipulation and arbitrage exploits.


---
