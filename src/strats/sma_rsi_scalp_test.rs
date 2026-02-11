#![allow(unused_variables)]
use super::*;
use TimeFrame::*;
use Value::*;

const SMA_RSI_LONG_THRESH: f64 = 40.0;
const STOCH_HIGH: f64 = 80.0;
const STOCH_ARM_LOW: f64 = 30.0;
const ENTRY_MARGIN_PCT: f64 = 95.0;

pub struct SmaRsiScalpTest {
    sma_rsi_5m: IndexId,
    stoch_rsi_1m: IndexId,
    armed_long: bool,
}

impl SmaRsiScalpTest {
    pub fn init() -> Self {
        let inds = Self::required_indicators_static();
        Self {
            sma_rsi_5m: inds[0],
            stoch_rsi_1m: inds[1],
            armed_long: false,
        }
    }
}

impl NeedsIndicators for SmaRsiScalpTest {
    fn required_indicators_static() -> Vec<IndexId> {
        vec![
            (
                IndicatorKind::SmaOnRsi {
                    periods: 12,
                    smoothing_length: 10,
                },
                Min5,
            ),
            (
                IndicatorKind::StochRsi {
                    periods: 14,
                    k_smoothing: Some(3),
                    d_smoothing: Some(3),
                },
                Min1,
            ),
        ]
    }
}

impl Strat for SmaRsiScalpTest {
    fn required_indicators(&self) -> Vec<IndexId> {
        Self::required_indicators_static()
    }

    fn on_idle(&mut self, ctx: StratContext, armed: Armed) -> Option<Intent> {
        let StratContext {
            last_price,
            indicators,
            ..
        } = ctx;

        if last_price.close <= 0.0 {
            return None;
        }

        if armed.is_none() {
            self.armed_long = false;
            let (k, d) = match indicators.get(&self.stoch_rsi_1m)?.value {
                StochRsiValue { k, d } => (k, d),
                _ => return None,
            };

            if k < STOCH_ARM_LOW && d < STOCH_ARM_LOW {
                self.armed_long = true;
                return Some(Intent::Arm(timedelta!(Min1, 9)));
            }

            return None;
        }

        if !self.armed_long {
            return Some(Intent::Disarm);
        }

        let sma_rsi = match indicators.get(&self.sma_rsi_5m)?.value {
            SmaRsiValue(v) => v,
            _ => return None,
        };

        let size = SizeSpec::MarginPct(ENTRY_MARGIN_PCT);
        if sma_rsi < SMA_RSI_LONG_THRESH {
            self.armed_long = false;
            return Some(Intent::open_market(size));
        }

        None
    }

    fn on_open(&mut self, ctx: StratContext, open_pos: &OpenPosInfo) -> Option<Intent> {
        let StratContext {
            last_price,
            indicators,
            ..
        } = ctx;

        let (k, d) = match indicators.get(&self.stoch_rsi_1m)?.value {
            StochRsiValue { k, d } => (k, d),
            _ => return None,
        };

        if last_price.close <= 0.0 {
            return None;
        }

        if k > STOCH_HIGH && d > STOCH_HIGH {
            return Some(Intent::flatten_market());
        }

        None
    }

    fn on_busy(&mut self, _ctx: StratContext, _busy: BusyType) -> Option<Intent> {
        None
    }
}
