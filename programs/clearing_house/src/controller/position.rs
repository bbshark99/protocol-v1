use anchor_lang::prelude::*;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::controller;
use crate::controller::amm::SwapDirection;
use crate::error::*;
use crate::math::collateral::calculate_updated_collateral;
use crate::math::position::calculate_base_asset_value_and_pnl;
use crate::math_error;
use crate::{Market, MarketPosition, User};
use solana_program::msg;

#[derive(Clone, Copy, BorshSerialize, BorshDeserialize, PartialEq)]
pub enum PositionDirection {
    Long,
    Short,
}

impl Default for PositionDirection {
    // UpOnly
    fn default() -> Self {
        PositionDirection::Long
    }
}

pub fn increase(
    direction: PositionDirection,
    new_quote_asset_notional_amount: u128,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult {
    if new_quote_asset_notional_amount == 0 {
        return Ok(());
    }

    // Update funding rate if this is a new position
    if market_position.base_asset_amount == 0 {
        market_position.last_cumulative_funding_rate = market.amm.cumulative_funding_rate;
        market_position.last_cumulative_repeg_rebate = match direction {
            PositionDirection::Long => market.amm.cumulative_repeg_rebate_long,
            PositionDirection::Short => market.amm.cumulative_repeg_rebate_short,
        };
        market.open_interest = market
            .open_interest
            .checked_add(1)
            .ok_or_else(math_error!())?;
    }

    market_position.quote_asset_amount = market_position
        .quote_asset_amount
        .checked_add(new_quote_asset_notional_amount)
        .ok_or_else(math_error!())?;

    let swap_direction = match direction {
        PositionDirection::Long => SwapDirection::Add,
        PositionDirection::Short => SwapDirection::Remove,
    };

    let base_asset_acquired = controller::amm::swap_quote_asset(
        &mut market.amm,
        new_quote_asset_notional_amount,
        swap_direction,
        now,
    )?;

    // update the position size on market and user
    market_position.base_asset_amount = market_position
        .base_asset_amount
        .checked_add(base_asset_acquired)
        .ok_or_else(math_error!())?;
    market.base_asset_amount = market
        .base_asset_amount
        .checked_add(base_asset_acquired)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_add(base_asset_acquired)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_add(base_asset_acquired)
            .ok_or_else(math_error!())?;
    }

    Ok(())
}

pub fn reduce<'info>(
    direction: PositionDirection,
    new_quote_asset_notional_amount: u128,
    user: &mut Account<'info, User>,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult {
    let swap_direction = match direction {
        PositionDirection::Long => SwapDirection::Add,
        PositionDirection::Short => SwapDirection::Remove,
    };
    let (base_asset_value_before, pnl_before) =
        calculate_base_asset_value_and_pnl(market_position, &market.amm)?;
    let base_asset_swapped = controller::amm::swap_quote_asset(
        &mut market.amm,
        new_quote_asset_notional_amount,
        swap_direction,
        now,
    )?;

    market_position.base_asset_amount = market_position
        .base_asset_amount
        .checked_add(base_asset_swapped)
        .ok_or_else(math_error!())?;

    market.open_interest = market
        .open_interest
        .checked_sub((market_position.base_asset_amount == 0) as u128)
        .ok_or_else(math_error!())?;
    market.base_asset_amount = market
        .base_asset_amount
        .checked_add(base_asset_swapped)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_add(base_asset_swapped)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_add(base_asset_swapped)
            .ok_or_else(math_error!())?;
    }

    let (base_asset_value_after, _) =
        calculate_base_asset_value_and_pnl(market_position, &market.amm)?;

    assert_eq!(base_asset_value_before > base_asset_value_after, true);

    let base_asset_value_change = (base_asset_value_before as i128)
        .checked_sub(base_asset_value_after as i128)
        .ok_or_else(math_error!())?
        .abs();

    let quote_asset_amount_closed = market_position
        .quote_asset_amount
        .checked_mul(base_asset_value_change.unsigned_abs())
        .ok_or_else(math_error!())?
        .checked_div(base_asset_value_before)
        .ok_or_else(math_error!())?;

    market_position.quote_asset_amount = market_position
        .quote_asset_amount
        .checked_sub(quote_asset_amount_closed)
        .ok_or_else(math_error!())?;

    let pnl = pnl_before
        .checked_mul(base_asset_value_change)
        .ok_or_else(math_error!())?
        .checked_div(base_asset_value_before as i128)
        .ok_or_else(math_error!())?;

    user.collateral = calculate_updated_collateral(user.collateral, pnl)?;

    Ok(())
}

pub fn close(
    user: &mut Account<User>,
    market: &mut Market,
    market_position: &mut MarketPosition,
    now: i64,
) -> ClearingHouseResult {
    // If user has no base asset, return early
    if market_position.base_asset_amount == 0 {
        return Ok(());
    }

    let swap_direction = if market_position.base_asset_amount > 0 {
        SwapDirection::Add
    } else {
        SwapDirection::Remove
    };

    let (_base_asset_value, pnl) =
        calculate_base_asset_value_and_pnl(&market_position, &market.amm)?;

    controller::amm::swap_base_asset(
        &mut market.amm,
        market_position.base_asset_amount.unsigned_abs(),
        swap_direction,
        now,
    )?;

    user.collateral = calculate_updated_collateral(user.collateral, pnl)?;
    market_position.last_cumulative_funding_rate = 0;
    market_position.last_cumulative_repeg_rebate = 0;

    market.open_interest = market
        .open_interest
        .checked_sub(1)
        .ok_or_else(math_error!())?;

    market_position.quote_asset_amount = 0;

    market.base_asset_amount = market
        .base_asset_amount
        .checked_sub(market_position.base_asset_amount)
        .ok_or_else(math_error!())?;

    if market_position.base_asset_amount > 0 {
        market.base_asset_amount_long = market
            .base_asset_amount_long
            .checked_sub(market_position.base_asset_amount)
            .ok_or_else(math_error!())?;
    } else {
        market.base_asset_amount_short = market
            .base_asset_amount_short
            .checked_sub(market_position.base_asset_amount)
            .ok_or_else(math_error!())?;
    }

    market_position.base_asset_amount = 0;

    Ok(())
}