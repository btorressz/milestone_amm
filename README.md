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

---
